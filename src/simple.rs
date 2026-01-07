use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tempfile::TempDir;
use tracing::{info, warn};

use crate::commands::run_command_async;
use crate::git::fetch_pr_ref;
use crate::github::{fetch_pr_info, post_github_comment};
use crate::nix::{
    build_attr_cache_friendly, build_package, delete_store_path, get_attr_path,
    prune_and_build_attr,
};
use crate::types::{AttrBuildResult, INTERMEDIATE_ATTRS};

/// Process a single PR in simple mode (single attribute)
///
/// If `full_rebuild` is true, uses the old behavior (prune store paths, rebuild with --substituters "").
/// If `full_rebuild` is false (default), uses cache-friendly mode (allow cache, verify with --check).
pub async fn process_pr(
    pr_num: u64,
    override_attr: Option<&String>,
    token: Option<&String>,
    nixpkgs: &PathBuf,
    system: &str,
    dry_run: bool,
    full_rebuild: bool,
) -> Result<bool> {
    info!("srcbot: Verifying source for PR #{}", pr_num);
    info!("Fetching PR info from: https://api.github.com/repos/NixOS/nixpkgs/pulls/{}", pr_num);

    // Fetch PR info
    let pr_info = fetch_pr_info(pr_num, token.map(|s| s.as_str())).await?;
    info!(
        "PR head: {} ({})",
        pr_info.head.sha,
        pr_info.head.ref_name
    );

    // Determine attribute
    let attr = match override_attr {
        Some(a) => a.clone(),
        None => {
            info!("Attribute not provided, parsing from PR title: '{}'", pr_info.title);
            if let Some((attr_part, _)) = pr_info.title.split_once(':') {
                attr_part.trim().to_string()
            } else {
                return Err(anyhow!(
                    "Could not parse attribute from PR title '{}'. Expected format 'attribute: description'. Please provide --attr manually.",
                    pr_info.title
                ));
            }
        }
    };
    info!("Target attribute: {}", attr);

    // Create a temporary worktree for the PR
    let temp_dir = TempDir::new()?;
    let worktree_path = temp_dir.path().join("nixpkgs");

    // Fetch the PR commit
    let remote = "https://github.com/NixOS/nixpkgs";
    let merge_sha = pr_info
        .merge_commit_sha
        .as_ref()
        .ok_or_else(|| anyhow!("PR has no merge commit (might not be mergeable)"))?;

    info!("Fetching PR merge commit: {}", merge_sha);
    fetch_pr_ref(nixpkgs, remote, merge_sha, "refs/srcbot/merge").await?;

    // Create worktree
    info!("Creating worktree at {:?}", worktree_path);
    run_command_async(
        "git",
        &[
            "-C",
            nixpkgs.to_str().unwrap(),
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            "refs/srcbot/merge",
        ],
    )
    .await?;

    // Build all intermediate attributes (src, goModules, cargoDeps, etc.)
    let mut intermediate_results: Vec<AttrBuildResult> = Vec::new();
    let mut all_intermediates_success = true;

    if full_rebuild {
        info!("Using full-rebuild mode (prune store paths, no cache)");
    } else {
        info!("Using cache-friendly mode (allow cache, verify with --check)");
    }

    for sub_attr in INTERMEDIATE_ATTRS {
        // Stop if a previous build failed
        if !all_intermediates_success {
            break;
        }

        let result = if full_rebuild {
            prune_and_build_attr(&worktree_path, &attr, sub_attr, system, &worktree_path).await
        } else {
            build_attr_cache_friendly(&worktree_path, &attr, sub_attr, system, &worktree_path).await
        };

        if !result.success() {
            all_intermediates_success = false;
        }

        intermediate_results.push(result);
    }

    // Find the first failed intermediate (if any) for the skip message
    let failed_intermediate = intermediate_results.iter().find(|r| r.attempted() && !r.success());

    // If all intermediate builds passed, try building the package
    let (pkg_code, pkg_logs, pkg_attempted, pkg_cmd) = if all_intermediates_success {
        if full_rebuild {
            // Full rebuild mode: prune and rebuild package
            // Try to get current package path and delete it
            if let Ok(pkg_path) = get_attr_path(&worktree_path, &attr, "", system).await {
                if std::path::Path::new(&pkg_path).exists() {
                    info!("Package exists at: {}", pkg_path);
                    if let Err(e) = delete_store_path(&pkg_path).await {
                        warn!("Warning: Could not delete package: {}", e);
                    }
                } else {
                    info!("Package not in store, will build fresh");
                }
            }

            // Build package
            let mut cmd = format!("nix build --print-build-logs --no-link --impure --expr '(import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}).{}'", worktree_path.display(), system, attr);

            let (mut c, mut l) = build_package(&worktree_path, &attr, system, false).await?;

            info!("Package build finished. Log length: {}. Checking for cache hit...", l.len());
            let built_locally = l.contains("building '/nix/store");
            if c == Some(0) && (l.contains("copying path") || l.contains("will be fetched") || !built_locally) {
                info!("Package was fetched from cache or already existed. Forcing rebuild to verify...");
                cmd = format!("nix build --print-build-logs --no-link --rebuild --impure --expr '(import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}).{}'", worktree_path.display(), system, attr);
                let (c2, l2) = build_package(&worktree_path, &attr, system, true).await?;
                c = c2;
                l = l2;
            }

            (c, l, true, cmd)
        } else {
            // Cache-friendly mode: build with cache, use --rebuild if cached
            let cmd = format!("nix build --print-build-logs --no-link --impure --expr '(import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}).{}' (+ --rebuild if cached)", worktree_path.display(), system, attr);

            let (mut c, mut l) = build_package(&worktree_path, &attr, system, false).await?;

            info!("Package build finished. Log length: {}. Checking for cache hit...", l.len());
            let built_locally = l.contains("building '/nix/store");
            if c == Some(0) && (l.contains("copying path") || l.contains("will be fetched") || !built_locally) {
                info!("Package was fetched from cache or already existed. Forcing rebuild to verify...");
                let (c2, l2) = build_package(&worktree_path, &attr, system, true).await?;
                // Combine logs
                l = format!("{}\n--- Running --rebuild to verify cached package ---\n{}", l, l2);
                c = c2;
            }

            (c, l, true, cmd)
        }
    } else {
        (None, String::new(), false, String::new())
    };

    // Clean up worktree
    let _ = run_command_async(
        "git",
        &[
            "-C",
            nixpkgs.to_str().unwrap(),
            "worktree",
            "remove",
            "-f",
            worktree_path.to_str().unwrap(),
        ],
    )
    .await;

    // Helper to format logs
    let format_log = |logs: &str, title: &str| -> String {
        if logs.trim().is_empty() {
            return format!("\n**{}**: No build logs available. That is suspicious.", title);
        }
        let line_count = logs.lines().count();
        if line_count <= 100 {
            format!("\n<details><summary>{} Logs</summary>\n\n```\n{}\n```\n</details>", title, logs)
        } else {
            let last_100: Vec<&str> = logs.lines().rev().take(100).collect();
            let last_100_ordered: Vec<&str> = last_100.into_iter().rev().collect();
            let snippet = last_100_ordered.join("\n");
            format!("\n<details><summary>{} Logs (Last 100 lines)</summary>\n\n```\n...\n{}\n```\n</details>", title, snippet)
        }
    };

    let format_status = |code: Option<i32>| -> String {
        match code {
            Some(c) => format!("Exit Code: {}", c),
            None => "Exit Code: None (Signal/Unknown)".to_string(),
        }
    };

    // Construct Message
    let pkg_success = pkg_code == Some(0);
    let overall_status = if all_intermediates_success && (!pkg_attempted || pkg_success) {
        "Source verification passed"
    } else {
        "Source verification FAILED"
    };

    let mut message = format!(
        "## srcbot: {}\n\n- PR: #{}\n- Attribute: `{}`\n- System: `{}`\n\n",
        overall_status, pr_num, attr, system
    );

    // Track step number dynamically
    let mut step_num = 1;

    // Add sections for each intermediate attribute that was attempted
    for result in &intermediate_results {
        if result.attempted() {
            message.push_str(&format!("### Step {}: Build `.{}`\n\n", step_num, result.name));
            step_num += 1;
            message.push_str(&format!("- **Command**: `{}`\n", result.cmd));
            message.push_str(&format!("- **Status**: {}\n", format_status(result.code)));
            message.push_str(&format_log(&result.logs, &format!("{} Build", result.name)));
            message.push_str("\n\n");
        }
    }

    // Package Build Section
    if pkg_attempted {
        message.push_str(&format!("### Step {}: Build Package\n\n", step_num));
        message.push_str(&format!("- **Command**: `{}`\n", pkg_cmd));
        message.push_str(&format!("- **Status**: {}\n", format_status(pkg_code)));
        message.push_str(&format_log(&pkg_logs, "Package Build"));
    } else if let Some(failed) = failed_intermediate {
        message.push_str(&format!("### Step {}: Build Package\n\n", step_num));
        message.push_str(&format!("_Skipped because {} build failed._", failed.name));
    }

    let exit_success = all_intermediates_success && (!pkg_attempted || pkg_success);

    // Post to GitHub if not dry run
    if !dry_run {
        if let Some(token_str) = token {
            post_github_comment(pr_num, token_str, &message).await?;
        } else {
            warn!("No GitHub token provided, skipping comment post");
        }
    } else {
        info!("Dry run mode, not posting to GitHub");
    }

    info!("\n{}", message);

    Ok(exit_success)
}
