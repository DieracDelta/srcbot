use anyhow::Result;
use std::fs;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::{info, warn};

use crate::full_eval_types::EvalJobOutput;
use crate::types::{FullEvalBuildResult, RunState};

/// Global custom save location (set once at startup)
static SAVE_LOCATION: OnceLock<Option<PathBuf>> = OnceLock::new();

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

/// Get the log directory for a PR, creating it if needed
pub fn get_log_dir(pr_num: u64) -> Result<PathBuf> {
    let log_dir = get_base_dir().join("logs").join(pr_num.to_string());
    fs::create_dir_all(&log_dir)?;
    Ok(log_dir)
}

/// Get the log directory for a specific attribute in fix-hash operations
/// Structure: {base_dir}/logs/{attr_safe}/
pub fn get_fix_hash_attr_log_dir(attr: &str) -> Result<PathBuf> {
    let attr_safe = attr.replace('.', "_").replace('/', "_");
    let log_dir = get_base_dir().join("logs").join(attr_safe);
    fs::create_dir_all(&log_dir)?;
    Ok(log_dir)
}

/// Save build logs locally to {base_dir}/logs/{pr_num}/
pub fn save_logs_locally(pr_num: u64, results: &[FullEvalBuildResult]) -> Result<PathBuf> {
    let log_dir = get_base_dir().join("logs").join(pr_num.to_string());

    // Remove old logs if they exist
    if log_dir.exists() {
        fs::remove_dir_all(&log_dir)?;
    }
    fs::create_dir_all(&log_dir)?;

    // Count successes and failures for summary
    let passed: Vec<_> = results.iter().filter(|r| r.package_success).collect();
    let failed: Vec<_> = results.iter().filter(|r| !r.package_success).collect();

    // Create summary.md
    let mut summary = format!(
        "## srcbot: Full Evaluation Results for PR #{}\n\n**Status**: {}/{} packages passed, {} failed\n\n",
        pr_num,
        passed.len(),
        results.len(),
        failed.len()
    );

    if !failed.is_empty() {
        summary.push_str(
            "### Failed Packages\n\n| Package | Failed Step |\n|---------|-------------|\n",
        );
        for result in &failed {
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");
            summary.push_str(&format!("| {} | {} |\n", result.attr, failed_step));
        }
        summary.push('\n');
    }

    if !passed.is_empty() {
        summary.push_str(&format!("### {} Packages Passed\n\n", passed.len()));
        for result in &passed {
            let steps: Vec<_> = result
                .intermediate_results
                .iter()
                .map(|(name, _, _)| name.as_str())
                .chain(std::iter::once("package"))
                .collect();
            summary.push_str(&format!("- {} ({})\n", result.attr, steps.join(", ")));
        }
    }

    fs::write(log_dir.join("summary.md"), &summary)?;

    // Write individual log files
    for result in results {
        let attr_safe = result.attr.replace('.', "_").replace('/', "_");

        // Intermediate logs
        for (name, _success, logs) in &result.intermediate_results {
            if !logs.trim().is_empty() {
                let log_file = log_dir.join(format!("{}.{}.log", attr_safe, name));
                fs::write(&log_file, logs)?;
            }
        }

        // Package log
        if !result.package_logs.trim().is_empty() {
            let pkg_log_file = log_dir.join(format!("{}.package.log", attr_safe));
            fs::write(&pkg_log_file, &result.package_logs)?;
        }
    }

    Ok(log_dir)
}
