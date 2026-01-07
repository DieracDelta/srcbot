use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use tempfile::TempDir;
use tokio::process::Command as TokioCommand;
use tracing::{info, warn};

use crate::types::{FullEvalBuildResult, PullRequest};

/// Fetch PR info from GitHub API
pub async fn fetch_pr_info(pr: u64, token: Option<&str>) -> Result<PullRequest> {
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/repos/NixOS/nixpkgs/pulls/{}", pr);
    let max_retries = 3;

    for attempt in 1..=max_retries {
        let mut request = client
            .get(&url)
            .header("User-Agent", "srcbot")
            .header("Accept", "application/vnd.github.v3+json");

        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        match request.send().await {
            Ok(response) => {
                if response.status().is_success() {
                    let pr_info: PullRequest = response.json().await?;
                    return Ok(pr_info);
                } else {
                    let status = response.status();
                    let text = response.text().await.unwrap_or_default();
                    let short_text = if text.len() > 200 {
                        format!("{}...", &text[..200])
                    } else {
                        text
                    };

                    if status.is_server_error() {
                        warn!("Attempt {}/{} failed with status {}: {}. Retrying...", attempt, max_retries, status, short_text);
                        if attempt < max_retries {
                             tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                             continue;
                        }
                    }

                    return Err(anyhow!(
                        "Failed to fetch PR info: {} {}",
                        status,
                        short_text
                    ));
                }
            }
            Err(e) => {
                warn!("Attempt {}/{} failed with error: {}. Retrying...", attempt, max_retries, e);
                if attempt < max_retries {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                } else {
                    return Err(e.into());
                }
            }
        }
    }

    Err(anyhow!("Max retries reached fetching PR info"))
}

/// Post a comment to a GitHub PR
pub async fn post_github_comment(pr: u64, token: &str, body: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.github.com/repos/NixOS/nixpkgs/issues/{}/comments",
        pr
    );

    let response = client
        .post(&url)
        .header("User-Agent", "srcbot")
        .header("Accept", "application/vnd.github.v3+json")
        .header("Authorization", format!("Bearer {}", token))
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Failed to post comment: {} {}",
            response.status(),
            response.text().await?,
        ));
    }

    info!("Posted comment to PR #{}", pr);
    Ok(())
}

/// Get GitHub token from gh CLI
pub async fn get_github_token() -> Option<String> {
    if let Ok(output) = TokioCommand::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .await
    {
        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// Get the last N lines from a string
fn last_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= n {
        s.to_string()
    } else {
        lines[lines.len() - n..].join("\n")
    }
}

/// Create a gist with build results and post comment to PR
pub async fn create_gist_and_comment(
    pr_num: u64,
    results: &[FullEvalBuildResult],
    token: &str,
    dry_run: bool,
) -> Result<()> {
    let temp_dir = TempDir::new()?;
    let mut files = Vec::new();

    // Count successes and failures
    let total = results.len();
    let passed: Vec<_> = results.iter().filter(|r| r.package_success).collect();
    let failed: Vec<_> = results.iter().filter(|r| !r.package_success).collect();

    // Create summary.md
    let mut summary = format!(
        "## srcbot: Full Evaluation Results for PR #{}\n\n**Status**: {}/{} packages passed, {} failed\n\n",
        pr_num,
        passed.len(),
        total,
        failed.len()
    );

    if !failed.is_empty() {
        summary.push_str("### Failed Packages\n\n| Package | Failed Step | Log |\n|---------|-------------|-----|\n");
        for result in &failed {
            // Find which step failed
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");
            let log_file = format!("{}.{}.log", result.attr.replace('.', "_"), failed_step);
            summary.push_str(&format!("| {} | {} | [{}]({}) |\n", result.attr, failed_step, log_file, log_file));
        }
        summary.push('\n');
    }

    if !passed.is_empty() {
        summary.push_str("<details>\n<summary>");
        summary.push_str(&format!("{} packages passed</summary>\n\n", passed.len()));
        summary.push_str("| Package | Steps Built |\n|---------|-------------|\n");
        for result in &passed {
            let steps: Vec<_> = result
                .intermediate_results
                .iter()
                .map(|(name, _, _)| name.as_str())
                .chain(std::iter::once("package"))
                .collect();
            summary.push_str(&format!("| {} | {} |\n", result.attr, steps.join(", ")));
        }
        summary.push_str("\n</details>\n");
    }

    let summary_path = temp_dir.path().join("summary.md");
    std::fs::write(&summary_path, &summary)?;
    files.push(summary_path);

    // Create log files for each result (only if non-empty, gh gist rejects blank files)
    for result in results {
        let attr_safe = result.attr.replace('.', "_");

        // Intermediate logs
        for (name, _success, logs) in &result.intermediate_results {
            let trimmed = last_n_lines(logs, 100);
            if !trimmed.trim().is_empty() {
                let log_file = temp_dir.path().join(format!("{}.{}.log", attr_safe, name));
                std::fs::write(&log_file, trimmed)?;
                files.push(log_file);
            }
        }

        // Package log
        let trimmed = last_n_lines(&result.package_logs, 100);
        if !trimmed.trim().is_empty() {
            let pkg_log_file = temp_dir.path().join(format!("{}.package.log", attr_safe));
            std::fs::write(&pkg_log_file, trimmed)?;
            files.push(pkg_log_file);
        }
    }

    if dry_run {
        info!("Dry run - would create gist with {} files", files.len());
        info!("Summary:\n{}", summary);
        return Ok(());
    }

    // Create gist using gh
    let mut args = vec![
        "gist".to_string(),
        "create".to_string(),
        "--public".to_string(),
        "-d".to_string(),
        format!("srcbot results for PR #{}", pr_num),
    ];
    for f in &files {
        args.push(f.to_str().unwrap().to_string());
    }

    let output = TokioCommand::new("gh")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("Failed to run gh gist create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh gist create failed: {}", stderr));
    }

    let gist_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!("Created gist: {}", gist_url);

    // Post short comment to PR
    let mut comment = format!(
        "## srcbot: Full Evaluation Results\n\n**{}/{} packages passed** ([full results]({}))\n\n",
        passed.len(),
        total,
        gist_url
    );

    if !failed.is_empty() {
        comment.push_str("### Failures\n");
        for result in &failed {
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");
            comment.push_str(&format!("- `{}`: {} build failed\n", result.attr, failed_step));
        }
    }

    post_github_comment(pr_num, token, &comment).await?;

    Ok(())
}
