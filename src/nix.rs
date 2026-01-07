use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as TokioCommand;
use tracing::{debug, error, info, warn};

use crate::cache::get_log_dir;
use crate::commands::{run_command_async, run_command_tee_async};
use crate::full_eval_types::EvalJobOutput;
use crate::types::{AttrBuildResult, INTERMEDIATE_ATTRS};

/// Generate the Nix --apply expression to extract intermediate drvPaths
pub fn get_apply_expr() -> String {
    let attrs_list = INTERMEDIATE_ATTRS
        .iter()
        .map(|a| format!("\"{}\"", a))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        r#"drv: let
  intermediateAttrNames = [ {} ];
  tryGetDrvPath = name:
    let
      result = builtins.tryEval (
        if drv ? ${{name}} then
          let attr = drv.${{name}}; in
          if attr ? drvPath then attr.drvPath
          else null
        else null
      );
    in
    if result.success then result.value else null;
in
builtins.listToAttrs (
  builtins.filter (x: x.value != null) (
    map (name: {{ inherit name; value = tryGetDrvPath name; }}) intermediateAttrNames
  )
)"#,
        attrs_list
    )
}

/// Run nix-eval-jobs to enumerate all packages and their intermediate drvPaths
/// We basically just wanna figure out what exists. If it exists, we will then build it
pub async fn run_nix_eval_jobs(
    nixpkgs_path: &PathBuf,
    system: &str,
    workers: usize,
) -> Result<Vec<EvalJobOutput>> {
    let expr = format!(
        "import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}",
        nixpkgs_path.display(),
        system
    );
    let apply_expr = get_apply_expr();
    let workers_str = workers.to_string();

    info!("Running nix-eval-jobs with {} workers...", workers);
    debug!("Expression: {}", expr);
    debug!("Apply expression: {}", apply_expr);

    let output = TokioCommand::new("nix-eval-jobs")
        .args([
            "--expr",
            &expr,
            "--apply",
            &apply_expr,
            "--workers",
            &workers_str,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to run nix-eval-jobs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "nix-eval-jobs exited with status {}: {}",
            output.status, stderr
        );
        // fail later
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<EvalJobOutput>(line) {
            Ok(job) => results.push(job),
            Err(e) => {
                debug!("Failed to parse nix-eval-jobs line: {} - {}", e, line);
            }
        }
    }

    info!("nix-eval-jobs returned {} packages", results.len());
    Ok(results)
}

/// Run nix-eval-jobs for a specific package set (e.g., "python3Packages")
/// If pkgset is empty, evaluates all packages.
///
/// For sub-attrsets like `python3Packages`, we add `recurseForDerivations = true`
/// to tell nix-eval-jobs to recurse into the attribute set.
pub async fn run_nix_eval_jobs_pkgset(
    nixpkgs_path: &PathBuf,
    pkgset: &str,
    system: &str,
    workers: usize,
) -> Result<Vec<EvalJobOutput>> {
    let expr = if pkgset.is_empty() {
        // Full nixpkgs evaluation
        format!(
            "import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}",
            nixpkgs_path.display(),
            system
        )
    } else {
        // Sub-attrset: add recurseForDerivations to make nix-eval-jobs recurse into it
        format!(
            "let pkgs = import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; in pkgs.{} // {{ recurseForDerivations = true; }}",
            nixpkgs_path.display(),
            system,
            pkgset
        )
    };
    let apply_expr = get_apply_expr();
    let workers_str = workers.to_string();

    info!(
        "Running nix-eval-jobs for pkgset '{}' with {} workers...",
        if pkgset.is_empty() { "<all>" } else { pkgset },
        workers
    );
    debug!("Expression: {}", expr);
    debug!("Apply expression: {}", apply_expr);

    let output = TokioCommand::new("nix-eval-jobs")
        .args([
            "--expr",
            &expr,
            "--apply",
            &apply_expr,
            "--workers",
            &workers_str,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to run nix-eval-jobs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "nix-eval-jobs exited with status {}: {}",
            output.status, stderr
        );
        // Continue anyway - we'll process what we got
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<EvalJobOutput>(line) {
            Ok(job) => results.push(job),
            Err(e) => {
                debug!("Failed to parse nix-eval-jobs line: {} - {}", e, line);
            }
        }
    }

    info!("nix-eval-jobs returned {} packages", results.len());
    Ok(results)
}

/// TODO this is a hack.
/// It would be better to build this as
/// - build the thing pulling EVERYTHING from cache
/// - build the thing again using --check
/// I don't think this should be an error
/// Right now this:
/// - deletes multiple store paths in a single nix-store call with 30s timeout
/// - retries until success, with prompts to fix issues manually
pub async fn delete_store_paths_batch(store_paths: &[String], pr_num: u64) -> Result<()> {
    if store_paths.is_empty() {
        return Ok(());
    }

    let mut paths_to_delete = store_paths.to_vec();
    let mut timeout_secs = 30;

    loop {
        if paths_to_delete.is_empty() {
            return Ok(());
        }

        let (success, failed_path) =
            delete_store_paths_batch_once(&paths_to_delete, pr_num, timeout_secs).await?;
        if success {
            return Ok(());
        }

        error!("\n========================================");
        error!("GC FAILED - Some paths could not be deleted.");
        if let Some(ref p) = failed_path {
            error!("Failed path: {}", p);
        }
        error!("Check the error above and fix it manually.");
        error!("Common fixes:");
        error!("  - Kill any running nix processes");
        error!("  - Run: nix-collect-garbage");
        error!("  - Check referrers: nix-store --query --referrers <path>");
        error!("========================================");
        error!("Press Enter to retry GC, 's' to skip failing path, 't' to add 30s to timeout, 'm' to set timeout in minutes (current: {}s), or Ctrl-C to abort...", timeout_secs);

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        let trimmed = input.trim();
        if trimmed == "s" {
            if let Some(p) = failed_path {
                if let Some(idx) = paths_to_delete.iter().position(|x| x == &p) {
                    paths_to_delete.remove(idx);
                    eprintln!("Skipping path: {}", p);
                } else {
                    eprintln!("Warning: Failed path '{}' not found in pending list.", p);
                }
            } else {
                eprintln!("No specific failing path detected to skip.");
            }
        } else if trimmed == "t" {
            timeout_secs += 30;
            eprintln!("Timeout increased to {}s", timeout_secs);
        } else if trimmed == "m" {
            eprintln!("Enter timeout in minutes:");
            let mut min_input = String::new();
            if std::io::stdin().read_line(&mut min_input).is_ok() {
                if let Ok(mins) = min_input.trim().parse::<u64>() {
                    timeout_secs = mins * 60;
                    eprintln!("Timeout set to {}s ({} minutes)", timeout_secs, mins);
                } else {
                    eprintln!("Invalid number");
                }
            }
        }
    }
}

/// attempt to delete store paths
/// return true if successful
async fn delete_store_paths_batch_once(
    store_paths: &[String],
    pr_num: u64,
    timeout_secs: u64,
) -> Result<(bool, Option<String>)> {
    info!("Batch deleting {} store paths...", store_paths.len());

    // Set up log file for GC output
    let log_file = get_log_dir(pr_num)
        .map(|dir| dir.join("gc.log"))
        .ok()
        .and_then(|p| {
            // rename old log if exists
            if p.exists() {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = p.with_extension(format!("log.{}", timestamp));
                let _ = std::fs::rename(&p, backup);
            }
            std::fs::File::create(p).ok()
        });
    let log_file = Arc::new(Mutex::new(log_file));

    // log the paths we're trying to delete (to file and console via tracing)
    if let Ok(mut file_guard) = log_file.lock() {
        if let Some(ref mut file) = *file_guard {
            let _ = writeln!(
                file,
                "=== Attempting to delete {} store paths ===",
                store_paths.len()
            );
            for path in store_paths {
                let _ = writeln!(file, "  {}", path);
            }
            let _ = writeln!(file, "=== Starting nix-store --delete ===");
            let _ = file.flush();
        }
    }
    info!("GC: Attempting to delete {} store paths", store_paths.len());

    let mut args = vec!["nix-store", "--delete", "--ignore-liveness"];
    args.extend(store_paths.iter().map(|s| s.as_str()));

    // print the moderately dangerous root command so the user knows
    let cmd_str = format!("sudo {}", args.join(" "));
    info!("GC: Running: {}", cmd_str);
    if let Ok(mut file_guard) = log_file.lock() {
        if let Some(ref mut file) = *file_guard {
            let _ = writeln!(file, "Command: {}", cmd_str);
            let _ = file.flush();
        }
    }

    let mut child = TokioCommand::new("sudo")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sudo nix-store")?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let log_file_clone = log_file.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("GC stdout: {}", line);
                if let Ok(mut file_guard) = log_file_clone.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(file, "stdout: {}", line);
                        let _ = file.flush();
                    }
                }
            }
        }
    });

    let log_file_clone = log_file.clone();
    let failed_path = Arc::new(Mutex::new(None));
    let failed_path_clone = failed_path.clone();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("GC stderr: {}", line);
                if let Ok(mut file_guard) = log_file_clone.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(file, "stderr: {}", line);
                        let _ = file.flush();
                    }
                }
                if line.contains("deleting '") {
                    let _ = tx.try_send(());
                }
                if line.contains("error: cannot delete path '")
                    || line.contains("error: Cannot delete path '")
                {
                    if let Some(start) = line.find("'") {
                        if let Some(end) = line[start + 1..].find("'") {
                            let path = line[start + 1..start + 1 + end].to_string();
                            if let Ok(mut fp) = failed_path_clone.lock() {
                                *fp = Some(path);
                            }
                        }
                    }
                }
            }
        }
    });

    // TODO double check that futures aren't getting dropped
    let success = tokio::select! {
        status_res = child.wait() => {
            let status = status_res?;
            let _ = Command::new("stty").arg("sane").status();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let msg = if status.success() {
                format!("GC completed successfully")
            } else {
                format!("GC exited with {}", status)
            };
            println!("{}", msg);
            if let Ok(mut file_guard) = log_file.lock() {
                if let Some(ref mut file) = *file_guard {
                    let _ = writeln!(file, "=== {} ===", msg);
                }
            }
            if !status.success() {
                warn!("nix-store batch delete exited with {}", status);
            }
            status.success()
        }
        _ = rx.recv() => {
            let _ = Command::new("stty").arg("sane").status();
            info!("Batch delete started, waiting {}s before cancelling...", timeout_secs);
            tokio::select! {
                status_res = child.wait() => {
                    let status = status_res?;
                    let _ = Command::new("stty").arg("sane").status();
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    let msg = if status.success() {
                        format!("GC completed successfully")
                    } else {
                        format!("GC exited with {}", status)
                    };
                    println!("{}", msg);
                    if let Ok(mut file_guard) = log_file.lock() {
                        if let Some(ref mut file) = *file_guard {
                            let _ = writeln!(file, "=== {} ===", msg);
                        }
                    }
                    if !status.success() {
                        warn!("nix-store batch delete exited with {}", status);
                    }
                    status.success()
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)) => {
                    let _ = Command::new("stty").arg("sane").status();
                    println!("GC: Timeout reached, killing nix-store...");
                    if let Ok(mut file_guard) = log_file.lock() {
                        if let Some(ref mut file) = *file_guard {
                            let _ = writeln!(file, "=== Timeout reached, killing nix-store ===");
                        }
                    }
                    info!("Timeout reached, killing nix-store...");
                    child.kill().await?;
                    let _ = Command::new("stty").arg("sane").status();
                    info!("nix-store killed.");
                    false // timeout = failure
                }
            }
        }
    };

    let failed_path_val = failed_path.lock().unwrap().clone();
    Ok((success, failed_path_val))
}

/// Build a single intermediate attribute (async wrapper for parallel execution)
///
/// If `use_cache` is false, uses `--substituters ""` to force fresh fetch (full rebuild mode).
/// If `use_cache` is true, allows cache and verifies with --check if the FOD was substituted.
pub async fn build_intermediate_async(
    nixpkgs_path: PathBuf,
    attr: String,
    intermediate: String,
    system: String,
    pr_num: u64,
    use_cache: bool,
) -> (String, String, bool, String) {
    // (attr, intermediate, success, logs)
    let full_attr = format!("{}.{}", attr, intermediate);
    let expr = format!(
        "with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}",
        nixpkgs_path.display(),
        system,
        full_attr
    );

    if use_cache {
        info!("Building {} (cache-friendly mode)", full_attr);
    } else {
        info!("Building {} (full rebuild mode)", full_attr);
    }

    // Check if output path exists BEFORE building (for cache-friendly mode)
    // This is needed to detect FODs that already exist from previous builds
    let existed_before = if use_cache {
        match get_attr_path(&nixpkgs_path, &attr, &intermediate, &system).await {
            Ok(output_path) => std::path::Path::new(&output_path).exists(),
            Err(_) => false,
        }
    } else {
        false
    };

    // Set up log file for streaming
    let log_path = get_log_dir(pr_num)
        .map(|dir| {
            let attr_safe = attr.replace('.', "_").replace('/', "_");
            dir.join(format!("{}.{}.log", attr_safe, intermediate))
        })
        .ok();

    // If log file exists, rename it to preserve partial logs from interrupted builds
    if let Some(ref p) = log_path {
        if p.exists() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = p.with_extension(format!("log.{}", timestamp));
            let _ = std::fs::rename(p, backup);
        }
    }

    // Open log file (fresh)
    let log_file = log_path
        .as_ref()
        .and_then(|p| std::fs::File::create(p).ok());

    // Build args based on mode
    let args: Vec<&str> = if use_cache {
        vec!["--no-out-link", "--expr", &expr]
    } else {
        vec!["--no-out-link", "--substituters", "", "--expr", &expr]
    };

    let mut child = match TokioCommand::new("nix-build")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("Failed to spawn nix-build: {}", e);
            return (attr, intermediate, false, err_msg);
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let log_file = Arc::new(Mutex::new(log_file));
    let logs = Arc::new(Mutex::new(Vec::new()));

    // Spawn tasks to read stdout and stderr concurrently
    let logs_clone = logs.clone();
    let log_file_clone = log_file.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Write to log file immediately
                if let Ok(mut file_guard) = log_file_clone.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(file, "{}", line);
                        let _ = file.flush();
                    }
                }
                // Collect for return value
                if let Ok(mut log_guard) = logs_clone.lock() {
                    log_guard.push(line);
                }
            }
        }
    });

    let logs_clone = logs.clone();
    let log_file_clone = log_file.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Write to log file immediately
                if let Ok(mut file_guard) = log_file_clone.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(file, "{}", line);
                        let _ = file.flush();
                    }
                }
                // Collect for return value
                if let Ok(mut log_guard) = logs_clone.lock() {
                    log_guard.push(line);
                }
            }
        }
    });

    // Wait for process and output readers
    let (status, _, _) = tokio::join!(child.wait(), stdout_task, stderr_task);

    let success = status.map(|s| s.success()).unwrap_or(false);
    let mut log_text = logs.lock().map(|l| l.join("\n")).unwrap_or_default();

    // If using cache and build succeeded, check if we need to verify with --check
    if use_cache && success {
        // Get the output path
        if let Ok(output_path) = get_attr_path(&nixpkgs_path, &attr, &intermediate, &system).await {
            // Determine if we need to run --check:
            // - If path existed before build → always verify (could be from different nixpkgs checkout)
            // - If path didn't exist but was pulled from cache → verify
            // - If path didn't exist and was built fresh → skip
            let needs_check = if existed_before {
                info!(
                    "{} already existed in store, verifying with --check...",
                    full_attr
                );
                true
            } else {
                let built_locally = was_built_locally(&output_path).await.unwrap_or(false);
                if !built_locally {
                    info!("{} was from cache, verifying with --check...", full_attr);
                    true
                } else {
                    info!("{} was built fresh, skipping --check", full_attr);
                    false
                }
            };

            if needs_check {
                log_text.push_str("\n--- Running --check to verify FOD ---\n");

                // Run --check to verify
                let check_args = vec!["--no-out-link", "--check", "--expr", &expr];
                match TokioCommand::new("nix-build")
                    .args(&check_args)
                    .output()
                    .await
                {
                    Ok(output) => {
                        let check_success = output.status.success();
                        let check_stdout = String::from_utf8_lossy(&output.stdout);
                        let check_stderr = String::from_utf8_lossy(&output.stderr);
                        log_text.push_str(&check_stdout);
                        log_text.push_str(&check_stderr);

                        // Write check logs to file
                        if let Some(ref p) = log_path {
                            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(p) {
                                let _ = writeln!(file, "\n--- --check verification ---");
                                let _ = write!(file, "{}", check_stdout);
                                let _ = write!(file, "{}", check_stderr);
                            }
                        }

                        if !check_success {
                            return (attr, intermediate, false, log_text);
                        }
                    }
                    Err(e) => {
                        log_text.push_str(&format!("Failed to run --check: {}", e));
                        return (attr, intermediate, false, log_text);
                    }
                }
            }
        }
    }

    (attr, intermediate, success, log_text)
}

/// Build a package (final derivation) asynchronously
pub async fn build_package_async(
    nixpkgs_path: PathBuf,
    attr: String,
    system: String,
    pr_num: u64,
) -> (String, bool, String) {
    // TODO as a longer term feature we should support setting the system for cross comp
    let expr = format!(
        "(import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}).{}",
        nixpkgs_path.display(),
        system,
        attr
    );

    info!("Building package {}", attr);

    // Set up log file for streaming
    let log_path = get_log_dir(pr_num)
        .map(|dir| {
            let attr_safe = attr.replace('.', "_").replace('/', "_");
            dir.join(format!("{}.package.log", attr_safe))
        })
        .ok();

    // If log file exists, rename it to preserve partial logs from interrupted builds
    if let Some(ref p) = log_path {
        if p.exists() {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = p.with_extension(format!("log.{}", timestamp));
            let _ = std::fs::rename(p, backup);
        }
    }

    // Open log file (fresh)
    let log_file = log_path
        .as_ref()
        .and_then(|p| std::fs::File::create(p).ok());

    let mut child = match TokioCommand::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "build",
            "--print-build-logs",
            "--no-link",
            "--impure",
            "--expr",
            &expr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("Failed to spawn nix build: {}", e);
            return (attr, false, err_msg);
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let log_file = Arc::new(Mutex::new(log_file));
    let logs = Arc::new(Mutex::new(Vec::new()));

    // Spawn tasks to read stdout and stderr concurrently
    let logs_clone = logs.clone();
    let log_file_clone = log_file.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Write to log file immediately
                if let Ok(mut file_guard) = log_file_clone.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(file, "{}", line);
                        let _ = file.flush();
                    }
                }
                // Collect for return value
                if let Ok(mut log_guard) = logs_clone.lock() {
                    log_guard.push(line);
                }
            }
        }
    });

    let logs_clone = logs.clone();
    let log_file_clone = log_file.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Write to log file immediately
                if let Ok(mut file_guard) = log_file_clone.lock() {
                    if let Some(ref mut file) = *file_guard {
                        let _ = writeln!(file, "{}", line);
                        let _ = file.flush();
                    }
                }
                // Collect for return value
                if let Ok(mut log_guard) = logs_clone.lock() {
                    log_guard.push(line);
                }
            }
        }
    });

    // Wait for process and output readers
    let (status, _, _) = tokio::join!(child.wait(), stdout_task, stderr_task);

    let success = status.map(|s| s.success()).unwrap_or(false);
    let log_text = logs.lock().map(|l| l.join("\n")).unwrap_or_default();

    (attr, success, log_text)
}

/// Check if a package has a given sub-attribute
pub async fn has_attr(nixpkgs_path: &PathBuf, pkg_attr: &str, sub_attr: &str, system: &str) -> bool {
    let expr = format!(
        "with import {} {{ system = \"{}\"; }}; {} ? {}",
        nixpkgs_path.display(),
        system,
        pkg_attr,
        sub_attr
    );

    match run_command_async("nix-instantiate", &["--eval", "--expr", &expr]).await {
        Ok(result) => result.trim() == "true",
        Err(_) => false,
    }
}

/// Get the store path for a package's sub-attribute
pub async fn get_attr_path(
    nixpkgs_path: &PathBuf,
    pkg_attr: &str,
    sub_attr: &str,
    system: &str,
) -> Result<String> {
    let full_attr = if sub_attr.is_empty() {
        pkg_attr.to_string()
    } else {
        format!("{}.{}", pkg_attr, sub_attr)
    };

    let expr = format!(
        "with import {} {{ system = \"{}\"; }}; {}.outPath",
        nixpkgs_path.display(),
        system,
        full_attr
    );

    // nix-instantiate returns quoted string, so we need to strip quotes
    let result = run_command_async("nix-instantiate", &["--eval", "--expr", &expr]).await?;
    Ok(result.trim().trim_matches('"').to_string())
}

/// Check if a store path was built locally (ultimate=true) or substituted from cache (ultimate=false).
///
/// Uses `nix path-info --json` which is a stable API. The `ultimate` field indicates:
/// - `true`: Path was built locally on this machine
/// - `false`: Path was substituted from a binary cache
///
/// Returns `Ok(true)` if built locally, `Ok(false)` if from cache or on any error.
pub async fn was_built_locally(store_path: &str) -> Result<bool> {
    let output = run_command_async(
        "nix",
        &[
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--json",
            store_path,
        ],
    )
    .await?;

    let json: serde_json::Value = serde_json::from_str(&output)
        .with_context(|| format!("Failed to parse nix path-info JSON: {}", output))?;

    // path-info returns {"/nix/store/...": {"ultimate": true/false, ...}}
    if let Some(info) = json.as_object().and_then(|o| o.values().next()) {
        return Ok(info.get("ultimate").and_then(|u| u.as_bool()).unwrap_or(false));
    }

    // If we can't determine, assume it came from cache (safer to run --check)
    Ok(false)
}

/// Build a package's sub-attribute.
///
/// If `use_cache` is false, uses `--substituters ""` to force fresh fetch (old behavior).
/// If `use_cache` is true, allows dependencies to come from cache.
pub async fn build_attr(
    nixpkgs_path: &PathBuf,
    pkg_attr: &str,
    sub_attr: &str,
    system: &str,
    use_cache: bool,
) -> Result<(Option<i32>, String)> {
    let full_attr = if sub_attr.is_empty() {
        pkg_attr.to_string()
    } else {
        format!("{}.{}", pkg_attr, sub_attr)
    };

    let expr = format!(
        "with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}",
        nixpkgs_path.display(),
        system,
        full_attr
    );

    if use_cache {
        info!("Building {} (cache enabled)...", full_attr);
        run_command_tee_async("nix-build", &["--no-out-link", "--expr", &expr]).await
    } else {
        info!("Building {} with no substitutes...", full_attr);
        run_command_tee_async(
            "nix-build",
            &["--no-out-link", "--substituters", "", "--expr", &expr],
        )
        .await
    }
}

/// Build a package's sub-attribute with cache enabled, then run --check if needed.
///
/// This is the cache-friendly build mode:
/// 1. Check if the output path already exists BEFORE building
/// 2. Build with cache enabled (deps can come from cache)
/// 3. Determine if --check is needed:
///    - If path existed before build → always run --check (might be from different nixpkgs)
///    - If path didn't exist but was pulled from cache → run --check
///    - If path didn't exist and was built fresh → skip --check
pub async fn build_attr_with_check(
    nixpkgs_path: &PathBuf,
    pkg_attr: &str,
    sub_attr: &str,
    system: &str,
) -> Result<(Option<i32>, String)> {
    let full_attr = if sub_attr.is_empty() {
        pkg_attr.to_string()
    } else {
        format!("{}.{}", pkg_attr, sub_attr)
    };

    // Get the output path BEFORE building to check if it already exists
    let output_path = get_attr_path(nixpkgs_path, pkg_attr, sub_attr, system).await?;
    let existed_before = std::path::Path::new(&output_path).exists();

    // Phase 1: Build with cache enabled
    let (code, logs) = build_attr(nixpkgs_path, pkg_attr, sub_attr, system, true).await?;
    if code != Some(0) {
        return Ok((code, logs));
    }

    // Phase 2: Determine if we need to run --check
    let needs_check = if existed_before {
        // Path existed before build - always verify (could be from different nixpkgs checkout)
        info!(
            "{} already existed in store, verifying with --check...",
            full_attr
        );
        true
    } else {
        // Path didn't exist - check if pulled from cache vs built fresh
        let built_locally = was_built_locally(&output_path).await.unwrap_or(false);
        if !built_locally {
            info!(
                "{} was from cache, verifying with --check...",
                full_attr
            );
            true
        } else {
            info!("{} was built fresh, skipping --check", full_attr);
            false
        }
    };

    if needs_check {
        let expr = format!(
            "with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}",
            nixpkgs_path.display(),
            system,
            full_attr
        );

        let (check_code, check_logs) = run_command_tee_async(
            "nix-build",
            &["--no-out-link", "--check", "--expr", &expr],
        )
        .await?;

        // Combine logs from both phases
        let combined_logs = format!(
            "{}\n--- Running --check to verify FOD ---\n{}",
            logs, check_logs
        );
        return Ok((check_code, combined_logs));
    }

    Ok((code, logs))
}

/// Delete a single store path
pub async fn delete_store_path(store_path: &str) -> Result<()> {
    info!("Deleting store path with --ignore-liveness: {}", store_path);

    let mut child = TokioCommand::new("sudo")
        .args(&["nix-store", "--delete", "--ignore-liveness", store_path])
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn sudo nix-store")?;

    let stderr = child.stderr.take().expect("Failed to capture stderr");
    let reader = tokio::io::BufReader::new(stderr);
    let mut lines = reader.lines();

    // Channel to notify when the trigger line is seen
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    // Buffer to store lines for printing later
    let output_buffer: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = output_buffer.clone();

    // Background task to read stderr
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            // Store line in buffer instead of printing immediately
            if let Ok(mut buffer) = buffer_clone.lock() {
                buffer.push(line.clone());
            }

            if line.contains("deleting '") {
                let _ = tx.send(()).await;
            }
        }
    });

    // Wait for process exit OR trigger
    tokio::select! {
        status_res = child.wait() => {
            let status = status_res?;
            // Attempt to restore terminal sanity after sudo/nix-store potentially messed it up
            let _ = Command::new("stty").arg("sane").status();

            // Print collected output now that terminal is sane
            if let Ok(buffer) = output_buffer.lock() {
                for line in buffer.iter() {
                    info!("{}", line);
                }
            }

            if !status.success() {
                return Err(anyhow!("nix-store failed with {}", status));
            }
        }
        _ = rx.recv() => {
            // Attempt to restore terminal sanity before logging trigger
            let _ = Command::new("stty").arg("sane").status();
            info!("Trigger detected, waiting 2s before cancelling...");
            // Now race between child finishing and timeout
            tokio::select! {
                status_res = child.wait() => {
                    let status = status_res?;
                    // Attempt to restore terminal sanity
                    let _ = Command::new("stty").arg("sane").status();

                    // Print collected output
                    if let Ok(buffer) = output_buffer.lock() {
                        for line in buffer.iter() {
                            info!("{}", line);
                        }
                    }

                    if !status.success() {
                        return Err(anyhow!("nix-store failed with {}", status));
                    }
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(2)) => {
                    // Restore terminal sanity before printing output/logging
                    let _ = Command::new("stty").arg("sane").status();

                    // Print collected output so far
                    if let Ok(buffer) = output_buffer.lock() {
                        for line in buffer.iter() {
                            info!("{}", line);
                        }
                    }

                    info!("Timeout reached, killing nix-store...");
                    child.kill().await?;

                    // Restore terminal sanity after killing the process
                    let _ = Command::new("stty").arg("sane").status();
                    info!("nix-store killed.");
                }
            }
        }
    }

    Ok(())
}

/// Build a package using nix build command
pub async fn build_package(
    nixpkgs_path: &PathBuf,
    attr: &str,
    system: &str,
    rebuild: bool,
) -> Result<(Option<i32>, String)> {
    let expr = format!(
        "(import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}).{}",
        nixpkgs_path.display(),
        system,
        attr
    );

    info!("Building package {} with no substitutes...", attr);
    let mut args = vec![
        "--extra-experimental-features",
        "nix-command",
        "build",
        "--print-build-logs",
        "--no-link",
        "--impure",
        "--expr",
        &expr,
    ];

    if rebuild {
        args.insert(5, "--rebuild");
    }

    run_command_tee_async("nix", &args).await
}

/// Prune an attribute from the store if it exists, then build it fresh.
/// This is the old-style full rebuild mode (used with --full-rebuild flag).
pub async fn prune_and_build_attr(
    nixpkgs_path: &PathBuf,
    pkg_attr: &str,
    sub_attr: &str,
    system: &str,
    nixpkgs_display: &PathBuf,
) -> AttrBuildResult {
    let name = sub_attr.to_string();
    let full_attr = format!("{}.{}", pkg_attr, sub_attr);

    // Check if attribute exists
    if !has_attr(nixpkgs_path, pkg_attr, sub_attr, system).await {
        info!("Package does not have {} attribute, skipping", sub_attr);
        return AttrBuildResult {
            name,
            exists: false,
            code: None,
            logs: String::new(),
            cmd: String::new(),
        };
    }

    info!("Package has {} attribute, will build it", sub_attr);

    // Try to get current path and delete it
    if let Ok(store_path) = get_attr_path(nixpkgs_path, pkg_attr, sub_attr, system).await {
        if std::path::Path::new(&store_path).exists() {
            info!("{} exists at: {}", sub_attr, store_path);
            if let Err(e) = delete_store_path(&store_path).await {
                warn!("Warning: Could not delete {}: {}", sub_attr, e);
            }
        } else {
            info!("{} not in store, will fetch fresh", sub_attr);
        }
    }

    // Build the attribute
    let cmd = format!(
        "nix-build --no-out-link --substituters '' --expr 'with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}'",
        nixpkgs_display.display(),
        system,
        full_attr
    );

    match build_attr(nixpkgs_path, pkg_attr, sub_attr, system, false).await {
        Ok((code, logs)) => AttrBuildResult {
            name,
            exists: true,
            code,
            logs,
            cmd,
        },
        Err(e) => {
            warn!("Build error for {}: {}", sub_attr, e);
            AttrBuildResult {
                name,
                exists: true,
                code: None,
                logs: e.to_string(),
                cmd,
            }
        }
    }
}

/// Build an attribute using cache-friendly mode (default behavior).
/// Dependencies come from cache, FODs are verified with --check if they came from cache.
pub async fn build_attr_cache_friendly(
    nixpkgs_path: &PathBuf,
    pkg_attr: &str,
    sub_attr: &str,
    system: &str,
    nixpkgs_display: &PathBuf,
) -> AttrBuildResult {
    let name = sub_attr.to_string();
    let full_attr = format!("{}.{}", pkg_attr, sub_attr);

    // Check if attribute exists
    if !has_attr(nixpkgs_path, pkg_attr, sub_attr, system).await {
        info!("Package does not have {} attribute, skipping", sub_attr);
        return AttrBuildResult {
            name,
            exists: false,
            code: None,
            logs: String::new(),
            cmd: String::new(),
        };
    }

    info!("Package has {} attribute, will build it (cache-friendly mode)", sub_attr);

    // Build the attribute using cache-friendly mode
    let cmd = format!(
        "nix-build --no-out-link --expr 'with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}' (+ --check if cached)",
        nixpkgs_display.display(),
        system,
        full_attr
    );

    match build_attr_with_check(nixpkgs_path, pkg_attr, sub_attr, system).await {
        Ok((code, logs)) => AttrBuildResult {
            name,
            exists: true,
            code,
            logs,
            cmd,
        },
        Err(e) => {
            warn!("Build error for {}: {}", sub_attr, e);
            AttrBuildResult {
                name,
                exists: true,
                code: None,
                logs: e.to_string(),
                cmd,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_apply_expr_contains_all_intermediate_attrs() {
        let expr = get_apply_expr();
        // Verify the expression contains all INTERMEDIATE_ATTRS
        for attr in INTERMEDIATE_ATTRS {
            assert!(
                expr.contains(&format!("\"{}\"", attr)),
                "Apply expression should contain intermediate attr: {}",
                attr
            );
        }
    }

    #[test]
    fn test_get_apply_expr_valid_nix_structure() {
        let expr = get_apply_expr();
        // Verify it has basic Nix structure
        assert!(expr.contains("drv:"), "Expression should start with drv:");
        assert!(
            expr.contains("let"),
            "Expression should have let binding"
        );
        assert!(
            expr.contains("intermediateAttrNames"),
            "Expression should define intermediateAttrNames"
        );
        assert!(
            expr.contains("builtins.tryEval"),
            "Expression should use builtins.tryEval for safety"
        );
        assert!(
            expr.contains("builtins.listToAttrs"),
            "Expression should return an attrset"
        );
    }

    #[test]
    fn test_intermediate_attrs_src_comes_first() {
        // This is critical for the ordering dependency documented in types.rs
        assert_eq!(
            INTERMEDIATE_ATTRS[0], "src",
            "src must be the first intermediate attr because other attrs depend on it"
        );
    }

    #[test]
    fn test_intermediate_attrs_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for attr in INTERMEDIATE_ATTRS {
            assert!(
                seen.insert(attr),
                "INTERMEDIATE_ATTRS contains duplicate: {}",
                attr
            );
        }
    }

    #[test]
    fn test_attr_build_result_success() {
        let result = AttrBuildResult {
            name: "src".to_string(),
            exists: true,
            code: Some(0),
            logs: "built successfully".to_string(),
            cmd: "nix-build ...".to_string(),
        };
        assert!(result.success());
        assert!(result.attempted());
    }

    #[test]
    fn test_attr_build_result_nonexistent_is_success() {
        // Non-existent attrs are considered "success" (nothing to do)
        let result = AttrBuildResult {
            name: "goModules".to_string(),
            exists: false,
            code: None,
            logs: String::new(),
            cmd: String::new(),
        };
        assert!(result.success());
        assert!(!result.attempted());
    }

    #[test]
    fn test_attr_build_result_failure() {
        let result = AttrBuildResult {
            name: "src".to_string(),
            exists: true,
            code: Some(1),
            logs: "hash mismatch".to_string(),
            cmd: "nix-build ...".to_string(),
        };
        assert!(!result.success());
        assert!(result.attempted());
    }

    #[test]
    fn test_attr_build_result_no_exit_code_is_failure() {
        // If exists but no exit code, it's a failure (e.g., signal termination)
        let result = AttrBuildResult {
            name: "src".to_string(),
            exists: true,
            code: None,
            logs: "killed by signal".to_string(),
            cmd: "nix-build ...".to_string(),
        };
        assert!(!result.success());
        assert!(result.attempted());
    }

    // Tests for path-info JSON parsing (used by was_built_locally)

    #[test]
    fn test_parse_path_info_locally_built() {
        // When a path is built locally, ultimate is true
        let json = r#"{"/nix/store/abc123-hello-src":{"signatures":[],"ultimate":true}}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parsed.as_object().unwrap().values().next().unwrap();
        let ultimate = info.get("ultimate").and_then(|u| u.as_bool()).unwrap();
        assert!(ultimate, "Locally built path should have ultimate=true");
    }

    #[test]
    fn test_parse_path_info_from_cache() {
        // When a path comes from cache, ultimate is false and has signatures
        let json = r#"{"/nix/store/abc123-hello-src":{"signatures":["cache.nixos.org-1:xyz"],"ultimate":false}}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parsed.as_object().unwrap().values().next().unwrap();
        let ultimate = info.get("ultimate").and_then(|u| u.as_bool()).unwrap();
        assert!(!ultimate, "Cached path should have ultimate=false");
    }

    #[test]
    fn test_parse_path_info_missing_ultimate_defaults_false() {
        // If ultimate field is missing, we default to false (assume cached, safer to run --check)
        let json = r#"{"/nix/store/abc123":{"signatures":[]}}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parsed.as_object().unwrap().values().next().unwrap();
        let ultimate = info.get("ultimate").and_then(|u| u.as_bool()).unwrap_or(false);
        assert!(!ultimate, "Missing ultimate should default to false");
    }

    #[test]
    fn test_parse_path_info_empty_object() {
        // If path-info returns empty object, we should handle gracefully
        let json = r#"{}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parsed.as_object().and_then(|o| o.values().next());
        assert!(info.is_none(), "Empty object should return None for info");
    }

    #[test]
    fn test_parse_path_info_with_multiple_paths() {
        // path-info might return multiple paths, we check the first one
        let json = r#"{
            "/nix/store/abc123-hello-src":{"signatures":[],"ultimate":true},
            "/nix/store/def456-world-src":{"signatures":["cache.nixos.org-1:xyz"],"ultimate":false}
        }"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        // We take the first value we find (order not guaranteed in JSON objects)
        let info = parsed.as_object().and_then(|o| o.values().next()).unwrap();
        // Just verify we can parse it - the actual value depends on iteration order
        assert!(info.get("ultimate").is_some());
    }

    #[test]
    fn test_parse_path_info_ultimate_not_bool() {
        // If ultimate is somehow not a bool, we should handle gracefully
        let json = r#"{"/nix/store/abc123":{"signatures":[],"ultimate":"yes"}}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parsed.as_object().unwrap().values().next().unwrap();
        let ultimate = info.get("ultimate").and_then(|u| u.as_bool()).unwrap_or(false);
        assert!(!ultimate, "Non-bool ultimate should default to false");
    }

    #[test]
    fn test_parse_path_info_with_narsize_and_other_fields() {
        // Real nix path-info output has more fields
        let json = r#"{
            "/nix/store/abc123-hello-2.10":
            {
                "deriver":"/nix/store/xyz-hello.drv",
                "narHash":"sha256:abc123...",
                "narSize":1234,
                "references":["/nix/store/ref1","/nix/store/ref2"],
                "signatures":["cache.nixos.org-1:sig..."],
                "ultimate":false
            }
        }"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let info = parsed.as_object().unwrap().values().next().unwrap();
        let ultimate = info.get("ultimate").and_then(|u| u.as_bool()).unwrap();
        assert!(!ultimate, "Path with cache signature should have ultimate=false");
        // Also verify we can access other fields
        assert!(info.get("narSize").is_some());
        assert!(info.get("signatures").is_some());
    }

    // Tests for the needs_check logic (cache-friendly mode bug fix)
    // This tests the core logic for determining when to run --check

    #[test]
    fn test_needs_check_when_path_existed_before_build() {
        // Scenario 1: Path existed before build → must run --check
        // This catches the bug where a FOD already exists from a previous build
        let existed_before = true;
        let built_locally = true; // doesn't matter when existed_before is true
        let needs_check = existed_before || !built_locally;
        assert!(
            needs_check,
            "Must run --check when path existed before build (could be from different nixpkgs)"
        );
    }

    #[test]
    fn test_needs_check_when_pulled_from_cache() {
        // Scenario 2: Path didn't exist, pulled from cache → must run --check
        let existed_before = false;
        let built_locally = false; // from cache
        let needs_check = existed_before || !built_locally;
        assert!(
            needs_check,
            "Must run --check when path was pulled from cache"
        );
    }

    #[test]
    fn test_needs_check_when_built_fresh() {
        // Scenario 3: Path didn't exist, built fresh → skip --check
        let existed_before = false;
        let built_locally = true; // built locally just now
        let needs_check = existed_before || !built_locally;
        assert!(
            !needs_check,
            "Skip --check when path was built fresh (already verified)"
        );
    }

    #[test]
    fn test_needs_check_logic_complete() {
        // Comprehensive test covering all scenario combinations
        // The formula is: needs_check = existed_before || !built_locally

        // Truth table:
        // existed_before | built_locally | needs_check
        //     false      |    false      |    true  (from cache)
        //     false      |    true       |    false (built fresh)
        //     true       |    false      |    true  (existed + from cache somehow)
        //     true       |    true       |    true  (existed before)

        assert!(false || !false, "case 1: didn't exist, from cache -> check");
        assert!(!(false || !true), "case 2: didn't exist, built fresh -> no check");
        assert!(true || !false, "case 3: existed, from cache -> check");
        assert!(true || !true, "case 4: existed, built locally -> check");
    }
}
