use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tracing::{info, warn};

use crate::full_eval_types::EvalJobOutput;
use crate::summary::build_summary_comment;
use crate::types::{FullEvalBuildResult, RunState};

/// Global custom save location (set once at startup)
static SAVE_LOCATION: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Cache for log directories per PR (to ensure same directory is used throughout a run)
static LOG_DIR_CACHE: OnceLock<Mutex<HashMap<u64, PathBuf>>> = OnceLock::new();

/// Initialize the global save location (again, only called once at startup)
pub fn init_save_location(location: Option<PathBuf>) {
    let _ = SAVE_LOCATION.set(location);
}

/// Get the base srcbot directory (either custom or default)
fn get_base_dir() -> PathBuf {
    if let Some(Some(loc)) = SAVE_LOCATION.get() {
        loc.clone()
    } else {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("srcbot")
    }
}

/// Get the cache directory for storing eval results
pub fn get_cache_dir() -> Result<PathBuf> {
    let cache_dir = get_base_dir().join("evals");
    fs::create_dir_all(&cache_dir)?;
    Ok(cache_dir)
}

/// Get the cache file path for a given commit hash
pub fn get_cache_path(commit: &str, system: &str) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;
    Ok(cache_dir.join(format!("{}_{}.json", commit, system)))
}

/// Load cached eval results for a commit
pub fn load_cached_eval(commit: &str, system: &str) -> Option<Vec<EvalJobOutput>> {
    let cache_path = get_cache_path(commit, system).ok()?;
    if !cache_path.exists() {
        return None;
    }

    let file = fs::File::open(&cache_path).ok()?;
    let reader = BufReader::new(file);
    match serde_json::from_reader(reader) {
        Ok(results) => {
            info!("Loaded cached eval for {} from {:?}", commit, cache_path);
            Some(results)
        }
        Err(e) => {
            warn!("Failed to load cache file {:?}: {}", cache_path, e);
            None
        }
    }
}

/// Save eval results to cache
pub fn save_eval_cache(commit: &str, system: &str, results: &[EvalJobOutput]) -> Result<()> {
    let cache_path = get_cache_path(commit, system)?;
    let file = fs::File::create(&cache_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer(writer, results)?;
    info!("Saved eval cache for {} to {:?}", commit, cache_path);
    Ok(())
}

/// Get the state directory for storing run state
pub fn get_state_dir() -> Result<PathBuf> {
    let state_dir = get_base_dir().join("state");
    fs::create_dir_all(&state_dir)?;
    Ok(state_dir)
}

/// Get the state file path for a given PR
pub fn get_state_path(pr_num: u64) -> Result<PathBuf> {
    let state_dir = get_state_dir()?;
    Ok(state_dir.join(format!("{}.json", pr_num)))
}

/// Load saved run state for a PR
pub fn load_run_state(pr_num: u64) -> Option<RunState> {
    let state_path = get_state_path(pr_num).ok()?;
    if !state_path.exists() {
        return None;
    }

    let file = fs::File::open(&state_path).ok()?;
    let reader = BufReader::new(file);
    match serde_json::from_reader(reader) {
        Ok(state) => {
            info!(
                "Loaded saved state for PR #{} from {:?}",
                pr_num, state_path
            );
            Some(state)
        }
        Err(e) => {
            warn!("Failed to load state file {:?}: {}", state_path, e);
            None
        }
    }
}

/// Save run state for a PR
pub fn save_run_state(state: &RunState) -> Result<()> {
    let state_path = get_state_path(state.pr_num)?;
    let file = fs::File::create(&state_path)?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, state)?;
    info!(
        "Saved run state for PR #{} to {:?}",
        state.pr_num, state_path
    );
    Ok(())
}

/// Delete run state for a PR (after successful completion)
pub fn delete_run_state(pr_num: u64) -> Result<()> {
    let state_path = get_state_path(pr_num)?;
    if state_path.exists() {
        fs::remove_file(&state_path)?;
        info!("Deleted state file for PR #{}", pr_num);
    }
    Ok(())
}

/// Get the log directory for a PR, creating it if needed.
/// If logs/{pr_num} already exists with files, creates logs/{pr_num}_{i} for the lowest i available.
/// The chosen directory is cached so subsequent calls in the same run return the same path.
pub fn get_log_dir(pr_num: u64) -> Result<PathBuf> {
    let cache = LOG_DIR_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache_guard = cache.lock().unwrap();

    // Return cached directory if we've already determined it this run
    if let Some(cached) = cache_guard.get(&pr_num) {
        return Ok(cached.clone());
    }

    let logs_base = get_base_dir().join("logs");
    let base_dir = logs_base.join(pr_num.to_string());

    // Check if base directory exists and has files
    let dir_has_files = base_dir.exists() && fs::read_dir(&base_dir)?.next().is_some();

    let log_dir = if dir_has_files {
        // Find the lowest i such that logs/{pr_num}_{i} doesn't exist
        let mut i = 1u32;
        loop {
            let candidate = logs_base.join(format!("{}_{}", pr_num, i));
            if !candidate.exists() {
                info!(
                    "Log directory {} already has files, using {} instead",
                    base_dir.display(),
                    candidate.display()
                );
                break candidate;
            }
            i += 1;
        }
    } else {
        base_dir
    };

    fs::create_dir_all(&log_dir)?;
    cache_guard.insert(pr_num, log_dir.clone());
    Ok(log_dir)
}

/// Save a single build log file with optional commit suffix
/// Used for saving base build logs during false positive checks
///
/// # Arguments
/// * `pr_num` - PR number for directory
/// * `attr` - Package attribute (e.g., "python3Packages.requests")
/// * `step` - Build step (e.g., "src", "goModules", or "package")
/// * `logs` - Log content
/// * `commit_suffix` - Optional commit hash suffix (e.g., "abc12345" for base builds)
/// * `system` - Optional system architecture (e.g., "x86_64-linux", "aarch64-linux")
///              When provided, prefixes the log filename for multi-arch support
///
/// # Returns
/// Path to the saved log file
///
/// # Log Naming
/// - Without system: `attr.step.log` or `attr.step.base-{commit}.log`
/// - With system: `system.attr.step.log` or `system.attr.step.base-{commit}.log`
pub fn save_single_log(
    pr_num: u64,
    attr: &str,
    step: &str,
    logs: &str,
    commit_suffix: Option<&str>,
    system: Option<&str>,
) -> Result<PathBuf> {
    let log_dir = get_log_dir(pr_num)?;
    let attr_safe = attr.replace('.', "_").replace('/', "_");

    let log_name = match (system, commit_suffix) {
        (Some(sys), Some(suffix)) => format!("{}.{}.{}.base-{}.log", sys, attr_safe, step, suffix),
        (Some(sys), None) => format!("{}.{}.{}.log", sys, attr_safe, step),
        (None, Some(suffix)) => format!("{}.{}.base-{}.log", attr_safe, step, suffix),
        (None, None) => format!("{}.{}.log", attr_safe, step),
    };

    let log_path = log_dir.join(&log_name);
    fs::write(&log_path, logs)?;
    Ok(log_path)
}

/// Save the CLI command to COMMAND.md in the log directory
///
/// # Arguments
/// * `pr_num` - PR number for directory
/// * `cli_command` - The CLI command that was run
///
/// # Returns
/// Path to the saved COMMAND.md file
pub fn save_command_file(pr_num: u64, cli_command: &str) -> Result<PathBuf> {
    let log_dir = get_log_dir(pr_num)?;
    let command_path = log_dir.join("COMMAND.md");
    let content = format!("# Command\n\n```bash\n{}\n```\n", cli_command);
    fs::write(&command_path, &content)?;
    info!("Saved command to {:?}", command_path);
    Ok(command_path)
}

/// Get the log directory for a specific attribute in fix-hash operations
/// Structure: {base_dir}/logs/{attr_safe}/
pub fn get_fix_hash_attr_log_dir(attr: &str) -> Result<PathBuf> {
    let attr_safe = attr.replace('.', "_").replace('/', "_");
    let log_dir = get_base_dir().join("logs").join(attr_safe);
    fs::create_dir_all(&log_dir)?;
    Ok(log_dir)
}

/// Save build logs locally to the log directory for this PR
/// Uses get_log_dir to ensure we save to the same directory where individual logs were saved
pub fn save_logs_locally(pr_num: u64, results: &[FullEvalBuildResult]) -> Result<PathBuf> {
    let log_dir = get_log_dir(pr_num)?;

    // Create summary.md using the same function as GitHub comments (includes multi-arch support)
    let summary = build_summary_comment(
        pr_num,
        results,
        None, // log_url_base
        None, // base_commit
        None, // head_commit
        None, // base_commit_short
        None, // cli_command
        None, // remote_config
    );
    fs::write(log_dir.join("summary.md"), &summary)?;

    // Write individual log files with system prefix for multi-arch support
    for result in results {
        let attr_safe = result.attr.replace('.', "_").replace('/', "_");
        let system = if result.system.is_empty() {
            "x86_64-linux"
        } else {
            &result.system
        };

        // Intermediate logs
        for (name, _success, logs) in &result.intermediate_results {
            if !logs.trim().is_empty() {
                let log_file = log_dir.join(format!("{}.{}.{}.log", system, attr_safe, name));
                fs::write(&log_file, logs)?;
            }
        }

        // Package log
        if !result.package_logs.trim().is_empty() {
            let pkg_log_file = log_dir.join(format!("{}.{}.package.log", system, attr_safe));
            fs::write(&pkg_log_file, &result.package_logs)?;
        }
    }

    Ok(log_dir)
}
