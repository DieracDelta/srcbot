use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as TokioCommand;
use tracing::info;

/// Run a command asynchronously and return its stdout
pub async fn run_command_async(cmd: &str, args: &[&str]) -> Result<String> {
    info!("$ {} {}", cmd, args.join(" "));
    let output = TokioCommand::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("Failed to execute {}", cmd))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "Command {} failed with status {}: {}",
            cmd,
            output.status,
            stderr
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a command asynchronously, tee output to stdout/stderr, and return exit code + captured output
pub async fn run_command_tee_async(cmd: &str, args: &[&str]) -> Result<(Option<i32>, String)> {
    info!("$ {} {}", cmd, args.join(" "));

    let mut child = TokioCommand::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to execute {}", cmd))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let output_log = Arc::new(Mutex::new(String::new()));

    let output_clone = output_log.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("{}", line);
                if let Ok(mut log) = output_clone.lock() {
                    log.push_str(&line);
                    log.push('\n');
                }
            }
        }
    });

    let output_clone = output_log.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("{}", line);
                if let Ok(mut log) = output_clone.lock() {
                    log.push_str(&line);
                    log.push('\n');
                }
            }
        }
    });

    let (status, _, _) = tokio::join!(child.wait(), stdout_task, stderr_task);

    let captured_log = output_log.lock().unwrap().clone();
    Ok((status?.code(), captured_log))
}

/// Result of a nix command execution with timeout
#[derive(Debug)]
pub enum NixCommandResult {
    /// Command completed with exit code and logs
    Completed { code: Option<i32>, logs: String },
    /// Command timed out after the specified duration
    TimedOut { logs: String },
    /// Command failed to execute
    Failed { error: String },
}

/// Run a nix command with timeout protection and streaming output.
///
/// This prevents pipe buffer deadlocks by using separate tasks to drain stdout/stderr,
/// and prevents infinite hangs by enforcing a timeout.
///
/// Arguments:
/// - `cmd`: The command to run (e.g., "nix-build", "nix-eval-jobs")
/// - `args`: Command arguments
/// - `timeout_secs`: Maximum time to wait before killing the process
/// - `prefix`: Optional prefix to prepend to each output line (e.g., "[pkg.src]")
/// - `log_path`: Optional path to write logs to in real-time (each line flushed immediately)
///
/// Returns a NixCommandResult indicating success, timeout, or failure.
pub async fn run_nix_command_with_timeout(
    cmd: &str,
    args: &[&str],
    timeout_secs: u64,
    prefix: Option<&str>,
    log_path: Option<&Path>,
) -> NixCommandResult {
    if let Some(p) = prefix {
        info!("[{}] $ {} {} (timeout: {}s)", p, cmd, args.join(" "), timeout_secs);
    } else {
        info!("$ {} {} (timeout: {}s)", cmd, args.join(" "), timeout_secs);
    }

    let child_result = TokioCommand::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child_result {
        Ok(c) => c,
        Err(e) => {
            return NixCommandResult::Failed {
                error: format!("Failed to spawn {}: {}", cmd, e),
            };
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let output_log = Arc::new(Mutex::new(String::new()));

    // Create log file if path provided
    let log_file: Option<Arc<Mutex<File>>> = log_path.and_then(|p| {
        // Ensure parent directory exists
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match File::create(p) {
            Ok(f) => Some(Arc::new(Mutex::new(f))),
            Err(e) => {
                tracing::warn!("Failed to create log file {:?}: {}", p, e);
                None
            }
        }
    });

    // Clone prefix and log_file for the spawned tasks
    let stdout_prefix = prefix.map(|s| s.to_string());
    let stderr_prefix = prefix.map(|s| s.to_string());
    let stdout_log_file = log_file.clone();
    let stderr_log_file = log_file.clone();

    let output_clone = output_log.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Format line with prefix
                let formatted = if let Some(ref p) = stdout_prefix {
                    format!("[{}] {}", p, line)
                } else {
                    line.clone()
                };

                // Print to console
                println!("{}", formatted);

                // Write to log file immediately
                if let Some(ref file) = stdout_log_file {
                    if let Ok(mut f) = file.lock() {
                        let _ = writeln!(f, "{}", formatted);
                        let _ = f.flush();
                    }
                }

                // Collect in memory
                if let Ok(mut log) = output_clone.lock() {
                    log.push_str(&line);
                    log.push('\n');
                }
            }
        }
    });

    let output_clone = output_log.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Format line with prefix
                let formatted = if let Some(ref p) = stderr_prefix {
                    format!("[{}] {}", p, line)
                } else {
                    line.clone()
                };

                // Print to console
                eprintln!("{}", formatted);

                // Write to log file immediately
                if let Some(ref file) = stderr_log_file {
                    if let Ok(mut f) = file.lock() {
                        let _ = writeln!(f, "{}", formatted);
                        let _ = f.flush();
                    }
                }

                // Collect in memory
                if let Ok(mut log) = output_clone.lock() {
                    log.push_str(&line);
                    log.push('\n');
                }
            }
        }
    });

    // Race between command completion and timeout
    let timeout_duration = std::time::Duration::from_secs(timeout_secs);
    let result = tokio::select! {
        status_result = child.wait() => {
            // Command completed - wait for output tasks to finish
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let captured_log = output_log.lock().unwrap().clone();

            match status_result {
                Ok(status) => NixCommandResult::Completed {
                    code: status.code(),
                    logs: captured_log,
                },
                Err(e) => NixCommandResult::Failed {
                    error: format!("Failed to wait for {}: {}", cmd, e),
                },
            }
        }
        _ = tokio::time::sleep(timeout_duration) => {
            // Timeout reached - kill the process
            tracing::warn!("{} timed out after {}s, killing process...", cmd, timeout_secs);
            let _ = child.kill().await;

            // Give output tasks a moment to drain any remaining output
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                async {
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                }
            ).await;

            let captured_log = output_log.lock().unwrap().clone();
            NixCommandResult::TimedOut { logs: captured_log }
        }
    };

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_run_command_async_success() {
        let result = run_command_async("echo", &["hello", "world"]).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_run_nix_command_with_timeout_success() {
        let result = run_nix_command_with_timeout("echo", &["test output"], 10, None, None).await;
        match result {
            NixCommandResult::Completed { code, logs } => {
                assert_eq!(code, Some(0));
                assert!(logs.contains("test output"));
            }
            _ => panic!("Expected Completed result"),
        }
    }

    #[tokio::test]
    async fn test_run_nix_command_with_timeout_exit_code() {
        let result = run_nix_command_with_timeout("sh", &["-c", "exit 42"], 10, None, None).await;
        match result {
            NixCommandResult::Completed { code, .. } => {
                assert_eq!(code, Some(42));
            }
            _ => panic!("Expected Completed result"),
        }
    }

    #[tokio::test]
    async fn test_run_nix_command_with_timeout_captures_stderr() {
        let result =
            run_nix_command_with_timeout("sh", &["-c", "echo error >&2"], 10, None, None).await;
        match result {
            NixCommandResult::Completed { code, logs } => {
                assert_eq!(code, Some(0));
                assert!(logs.contains("error"));
            }
            _ => panic!("Expected Completed result"),
        }
    }

    #[tokio::test]
    async fn test_run_nix_command_with_timeout_times_out() {
        // Sleep for 5 seconds but timeout after 1
        let result = run_nix_command_with_timeout("sleep", &["5"], 1, None, None).await;
        match result {
            NixCommandResult::TimedOut { .. } => {
                // Expected
            }
            other => panic!("Expected TimedOut result, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_run_nix_command_with_timeout_nonexistent_command() {
        let result =
            run_nix_command_with_timeout("nonexistent_command_xyz_123", &[], 10, None, None).await;
        match result {
            NixCommandResult::Failed { error } => {
                assert!(error.contains("Failed to spawn"));
            }
            _ => panic!("Expected Failed result"),
        }
    }

    #[tokio::test]
    async fn test_run_command_async_captures_stdout() {
        let result = run_command_async("printf", &["line1\\nline2\\nline3"]).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
    }

    #[tokio::test]
    async fn test_run_command_async_failure() {
        let result = run_command_async("false", &[]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed"));
    }

    #[tokio::test]
    async fn test_run_command_async_nonexistent_command() {
        let result = run_command_async("nonexistent_command_xyz_123", &[]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_command_tee_async_success() {
        let result = run_command_tee_async("echo", &["test output"]).await;
        assert!(result.is_ok());
        let (code, logs) = result.unwrap();
        assert_eq!(code, Some(0));
        assert!(logs.contains("test output"));
    }

    #[tokio::test]
    async fn test_run_command_tee_async_exit_code() {
        let result = run_command_tee_async("sh", &["-c", "exit 42"]).await;
        assert!(result.is_ok());
        let (code, _) = result.unwrap();
        assert_eq!(code, Some(42));
    }

    #[tokio::test]
    async fn test_run_command_tee_async_captures_stderr() {
        let result = run_command_tee_async("sh", &["-c", "echo error >&2"]).await;
        assert!(result.is_ok());
        let (code, logs) = result.unwrap();
        assert_eq!(code, Some(0));
        assert!(logs.contains("error"));
    }

    #[tokio::test]
    async fn test_run_command_tee_async_captures_both_streams() {
        let result =
            run_command_tee_async("sh", &["-c", "echo stdout; echo stderr >&2"]).await;
        assert!(result.is_ok());
        let (code, logs) = result.unwrap();
        assert_eq!(code, Some(0));
        assert!(logs.contains("stdout"));
        assert!(logs.contains("stderr"));
    }
}
