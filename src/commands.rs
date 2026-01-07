use anyhow::{anyhow, Context, Result};
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
