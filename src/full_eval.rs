use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tracing::{info, warn};

use crate::cache::{
    delete_run_state, load_cached_eval, load_run_state, save_eval_cache, save_logs_locally,
    save_run_state, save_single_log,
};
use crate::commands::run_command_async;
use crate::full_eval_types::EvalJobOutput;
use crate::git::fetch_pr_ref;
use crate::github::{create_gist_and_comment, fetch_pr_info, post_github_comment};
use crate::nix::{
    build_intermediate_async, build_package_async, delete_store_paths_batch, get_attr_path,
    run_nix_eval_jobs,
};
use crate::summary::{build_intermediate_summary, build_summary_comment};
use crate::types::{
    ChangedPackage, ChangedPackageSer, FullEvalBuildResult, RunState, INTERMEDIATE_ATTRS,
};

/// Find packages whose intermediate drvPaths changed between base and PR.
/// If `verify_full_drvs` is true, also include packages where only the final drvPath changed.
pub fn find_changed_packages(
    base: &[EvalJobOutput],
    pr: &[EvalJobOutput],
    verify_full_drvs: bool,
) -> Vec<ChangedPackage> {
    // Build lookup map for base packages
    let base_map: HashMap<&str, &EvalJobOutput> =
        base.iter().map(|p| (p.attr.as_str(), p)).collect();

    let mut changed = Vec::new();

    for pr_pkg in pr {
        // Skip packages with errors
        if pr_pkg.error.is_some() {
            continue;
        }

        let pr_intermediates = pr_pkg.extra_value.as_ref();

        // If package is new or intermediates changed, add it
        let base_pkg = base_map.get(pr_pkg.attr.as_str());
        let base_intermediates = base_pkg.and_then(|p| p.extra_value.as_ref());

        let mut changed_intermediates = Vec::new();
        let mut intermediate_drv_paths = HashMap::new();

        // Check intermediate changes (if package has intermediates)
        if let Some(pr_ints) = pr_intermediates {
            for (name, pr_drv) in pr_ints {
                let pr_drv = match pr_drv {
                    Some(d) => d,
                    None => continue,
                };

                let base_drv = base_intermediates
                    .and_then(|m| m.get(name))
                    .and_then(|v| v.as_ref());

                // Changed if: new intermediate, or drvPath differs
                let is_changed = match base_drv {
                    Some(bd) => bd != pr_drv,
                    None => true,
                };

                if is_changed {
                    changed_intermediates.push(name.clone());
                    intermediate_drv_paths.insert(name.clone(), pr_drv.clone());
                }
            }
        }

        if !changed_intermediates.is_empty() {
            // Sort intermediates by the order in INTERMEDIATE_ATTRS
            changed_intermediates.sort_by_key(|name| {
                INTERMEDIATE_ATTRS
                    .iter()
                    .position(|&a| a == name)
                    .unwrap_or(usize::MAX)
            });

            changed.push(ChangedPackage {
                attr: pr_pkg.attr.clone(),
                changed_intermediates,
                intermediate_drv_paths,
                final_drv_changed: false,
            });
        } else if verify_full_drvs {
            // No intermediate changes - check if final drvPath changed
            let base_drv = base_pkg.and_then(|p| p.drv_path.as_ref());
            let pr_drv = pr_pkg.drv_path.as_ref();

            if let Some(pr_d) = pr_drv {
                let final_changed = match base_drv {
                    Some(bd) => bd != pr_d,
                    None => true, // New package
                };

                if final_changed {
                    changed.push(ChangedPackage {
                        attr: pr_pkg.attr.clone(),
                        changed_intermediates: vec![],
                        intermediate_drv_paths: HashMap::new(),
                        final_drv_changed: true,
                    });
                }
            }
        }
    }

    let intermediate_count = changed.iter().filter(|p| !p.changed_intermediates.is_empty()).count();
    let final_only_count = changed.iter().filter(|p| p.final_drv_changed).count();

    info!(
        "Found {} packages with changed intermediates, {} with only final drvPath changed",
        intermediate_count,
        final_only_count
    );
    changed
}

/// Process a PR in full-eval mode
///
/// If `full_rebuild` is true, uses the old behavior (prune store paths, rebuild with --substituters "").
/// If `full_rebuild` is false (default), uses cache-friendly mode (allow cache, verify with --check).
/// If `false_positive` is true, when a build fails, check if it also fails on the base branch.
/// If `verify_full_drvs` is true, also detect packages where the final drvPath changed (not just intermediates).
pub async fn process_pr_full_eval(
    pr_num: u64,
    token: Option<&String>,
    nixpkgs: &PathBuf,
    system: &str,
    eval_workers: usize,
    dry_run: bool,
    post_gist: bool,
    build_jobs: usize,
    resume: bool,
    full_rebuild: bool,
    false_positive: bool,
    verify_full_drvs: bool,
) -> Result<bool> {
    info!("srcbot: Full evaluation mode for PR #{}", pr_num);

    // Check for saved state if resuming
    let saved_state = if resume { load_run_state(pr_num) } else { None };

    // Fetch PR info
    let pr_info = fetch_pr_info(pr_num, token.map(|s| s.as_str())).await?;
    info!("PR: {} ({})", pr_info.title, pr_info.head.sha);

    // Get the correct commits to compare:
    // We want to compare the merge-base (where PR diverged from target) vs PR head
    // This isolates just the PR's changes, ignoring any changes that landed in master since the PR was opened
    let remote = "https://github.com/NixOS/nixpkgs";
    let pr_head_sha = &pr_info.head.sha;
    let target_branch_sha = &pr_info.base.sha;

    // Fetch both the target branch and PR head
    info!("Fetching target branch commit: {}", target_branch_sha);
    fetch_pr_ref(nixpkgs, remote, target_branch_sha, "refs/srcbot/target").await?;
    info!("Fetching PR head commit: {}", pr_head_sha);
    fetch_pr_ref(nixpkgs, remote, pr_head_sha, "refs/srcbot/pr-head").await?;

    // Compute the merge-base: the point where the PR branch diverged from the target branch
    let merge_base = run_command_async(
        "git",
        &[
            "-C",
            nixpkgs.to_str().unwrap(),
            "merge-base",
            "refs/srcbot/target",
            "refs/srcbot/pr-head",
        ],
    )
    .await?;
    info!("Merge-base (fork point): {}", merge_base);

    // Check if saved state is valid (matches current PR state)
    let valid_saved_state = saved_state.as_ref().and_then(|state| {
        if state.merge_base == merge_base
            && state.pr_head_sha == *pr_head_sha
            && state.system == system
        {
            info!(
                "Resuming from saved state: {} completed, {} remaining",
                state.completed_results.len(),
                state.packages_to_build.len() - state.completed_results.len()
            );
            Some(state)
        } else {
            warn!("Saved state doesn't match current PR state, starting fresh");
            None
        }
    });

    // Create temp dir and PR worktree (always needed)
    let temp_dir = TempDir::new()?;
    let base_path = temp_dir.path().join("base");
    let pr_path = temp_dir.path().join("pr");

    // Track whether base worktree exists (for false positive checks)
    let base_worktree_exists = Arc::new(Mutex::new(false));

    info!("Creating PR worktree (PR head) at {:?}", pr_path);
    run_command_async(
        "git",
        &[
            "-C",
            nixpkgs.to_str().unwrap(),
            "worktree",
            "add",
            pr_path.to_str().unwrap(),
            "refs/srcbot/pr-head",
        ],
    )
    .await?;

    // Either use saved state or compute fresh
    let (changed, completed_results, saved_intermediate_results, intermediates_already_posted): (
        Vec<ChangedPackage>,
        Vec<FullEvalBuildResult>,
        HashMap<String, Vec<(String, bool, String)>>,
        bool,
    ) = if let Some(state) = valid_saved_state {
        // Convert saved state back to working format
        let changed: Vec<ChangedPackage> = state
            .packages_to_build
            .iter()
            .map(|p| {
                ChangedPackage {
                    attr: p.attr.clone(),
                    changed_intermediates: p.changed_intermediates.clone(),
                    intermediate_drv_paths: HashMap::new(), // Not needed for building
                    final_drv_changed: p.final_drv_changed,
                }
            })
            .collect();
        (
            changed,
            state.completed_results.clone(),
            state.intermediate_results.clone(),
            state.intermediates_posted,
        )
    } else {
        // Fresh evaluation
        // Try to load cached base eval results, otherwise evaluate
        let base_packages = if let Some(cached) = load_cached_eval(&merge_base, system) {
            info!("Using cached base eval ({} packages)", cached.len());
            cached
        } else {
            info!("Creating base worktree (merge-base) at {:?}", base_path);
            run_command_async(
                "git",
                &[
                    "-C",
                    nixpkgs.to_str().unwrap(),
                    "worktree",
                    "add",
                    base_path.to_str().unwrap(),
                    &merge_base,
                ],
            )
            .await?;

            // Mark base worktree as existing
            *base_worktree_exists.lock().unwrap() = true;

            info!("Evaluating base nixpkgs...");
            let packages = run_nix_eval_jobs(&base_path, system, eval_workers).await?;

            // Cache the results
            if let Err(e) = save_eval_cache(&merge_base, system, &packages) {
                warn!("Failed to save eval cache: {}", e);
            }

            // Cleanup base worktree only if we don't need it for false positive checks
            if !false_positive {
                let _ = run_command_async(
                    "git",
                    &[
                        "-C",
                        nixpkgs.to_str().unwrap(),
                        "worktree",
                        "remove",
                        "-f",
                        base_path.to_str().unwrap(),
                    ],
                )
                .await;
            }

            packages
        };

        // Try to load cached PR eval results, otherwise evaluate
        let pr_packages = if let Some(cached) = load_cached_eval(pr_head_sha, system) {
            info!("Using cached PR eval ({} packages)", cached.len());
            cached
        } else {
            info!("Evaluating PR nixpkgs...");
            let packages = run_nix_eval_jobs(&pr_path, system, eval_workers).await?;

            // Cache the results
            if let Err(e) = save_eval_cache(pr_head_sha, system, &packages) {
                warn!("Failed to save PR eval cache: {}", e);
            }

            packages
        };

        // Find changed packages
        let changed = find_changed_packages(&base_packages, &pr_packages, verify_full_drvs);
        (changed, Vec::new(), HashMap::new(), false)
    };

    if changed.is_empty() {
        let msg = if verify_full_drvs {
            "No packages with changed intermediates or drvPaths found!"
        } else {
            "No packages with changed intermediates found!"
        };
        info!("{}", msg);

        // Cleanup PR worktree (base worktree already cleaned up or never created due to cache)
        let _ = run_command_async(
            "git",
            &[
                "-C",
                nixpkgs.to_str().unwrap(),
                "worktree",
                "remove",
                "-f",
                pr_path.to_str().unwrap(),
            ],
        )
        .await;

        if !dry_run {
            if let Some(token_str) = token {
                let comment_msg = if verify_full_drvs {
                    "## srcbot: Full Evaluation Results\n\nNo packages with changed intermediates or drvPaths detected."
                } else {
                    "## srcbot: Full Evaluation Results\n\nNo packages with changed source intermediates detected."
                };
                post_github_comment(pr_num, token_str, comment_msg).await?;
            }
        }
        return Ok(true);
    }

    // Get set of already completed package attrs
    let completed_attrs: std::collections::HashSet<String> =
        completed_results.iter().map(|r| r.attr.clone()).collect();

    // Filter to only packages that haven't been completed yet
    let remaining_packages: Vec<_> = changed
        .iter()
        .filter(|p| !completed_attrs.contains(&p.attr))
        .collect();

    info!(
        "Found {} packages total, {} already completed, {} remaining",
        changed.len(),
        completed_attrs.len(),
        remaining_packages.len()
    );

    for pkg in &remaining_packages {
        if pkg.final_drv_changed {
            info!("  - {}: [final drvPath changed]", pkg.attr);
        } else {
            info!("  - {}: {:?}", pkg.attr, pkg.changed_intermediates);
        }
    }

    // Create shared state for saving progress
    let run_state = Arc::new(Mutex::new(RunState {
        pr_num,
        merge_base: merge_base.clone(),
        pr_head_sha: pr_head_sha.clone(),
        system: system.to_string(),
        packages_to_build: changed
            .iter()
            .map(|p| ChangedPackageSer {
                attr: p.attr.clone(),
                changed_intermediates: p.changed_intermediates.clone(),
                final_drv_changed: p.final_drv_changed,
            })
            .collect(),
        intermediate_results: saved_intermediate_results.clone(),
        completed_results: completed_results.clone(),
        intermediates_posted: intermediates_already_posted,
    }));

    // Set up Ctrl-C handler to save state
    let state_for_handler = run_state.clone();
    let handler_pr_num = pr_num;
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            eprintln!("\nInterrupted! Saving state...");
            if let Ok(state) = state_for_handler.lock() {
                if let Err(e) = save_run_state(&state) {
                    eprintln!("Failed to save state: {}", e);
                } else {
                    eprintln!("State saved. Resume with --resume flag.");
                }
            }
            // Also save logs for what we have so far
            if let Ok(state) = state_for_handler.lock() {
                if !state.completed_results.is_empty() {
                    let _ = save_logs_locally(handler_pr_num, &state.completed_results);
                }
            }
            std::process::exit(130); // Standard exit code for SIGINT
        }
    });

    // Only prune store paths in full_rebuild mode
    if full_rebuild {
        info!("Using full-rebuild mode (prune store paths, no cache)");

        // Collect all store paths to prune (only for remaining packages)
        let mut paths_to_prune: Vec<String> = Vec::new();
        for pkg in &remaining_packages {
            // Prune each changed intermediate
            for intermediate in &pkg.changed_intermediates {
                if let Ok(out_path) = get_attr_path(&pr_path, &pkg.attr, intermediate, system).await
                {
                    if std::path::Path::new(&out_path).exists() {
                        paths_to_prune.push(out_path);
                    }
                }
            }
            // Also prune the final package
            if let Ok(out_path) = get_attr_path(&pr_path, &pkg.attr, "", system).await {
                if std::path::Path::new(&out_path).exists() {
                    paths_to_prune.push(out_path);
                }
            }
        }

        // Batch prune
        if !paths_to_prune.is_empty() {
            delete_store_paths_batch(&paths_to_prune, pr_num).await?;
        }
    } else {
        info!("Using cache-friendly mode (allow cache, verify with --check)");
    }

    // Build intermediates tier by tier (only for remaining packages)
    // Initialize results with any saved intermediate progress
    let mut results: HashMap<String, FullEvalBuildResult> = remaining_packages
        .iter()
        .map(|p| {
            let saved_intermediates = saved_intermediate_results
                .get(&p.attr)
                .cloned()
                .unwrap_or_default();
            (
                p.attr.clone(),
                FullEvalBuildResult {
                    attr: p.attr.clone(),
                    intermediate_results: saved_intermediates,
                    package_success: false,
                    package_logs: String::new(),
                    is_false_positive: false,
                },
            )
        })
        .collect();

    for intermediate in INTERMEDIATE_ATTRS {
        // Filter to packages that need this intermediate AND haven't built it yet
        let packages_needing: Vec<_> = remaining_packages
            .iter()
            .filter(|p| {
                // Check if this package needs this intermediate
                if !p.changed_intermediates.contains(&intermediate.to_string()) {
                    return false;
                }
                // Check if we already have a result for this intermediate (from resume)
                if let Some(result) = results.get(&p.attr) {
                    if result
                        .intermediate_results
                        .iter()
                        .any(|(name, _, _)| name == intermediate)
                    {
                        return false; // Already built
                    }
                }
                true
            })
            .collect();

        if packages_needing.is_empty() {
            continue;
        }

        info!(
            "Building {} packages' .{} attribute...",
            packages_needing.len(),
            intermediate
        );

        use futures::stream::{self, StreamExt};

        // Use buffer_unordered to stream results as they complete
        // use_cache = !full_rebuild (cache-friendly when not doing full rebuild)
        let use_cache = !full_rebuild;
        let mut stream = stream::iter(packages_needing.iter().map(|pkg| {
            build_intermediate_async(
                pr_path.clone(),
                pkg.attr.clone(),
                intermediate.to_string(),
                system.to_string(),
                pr_num,
                use_cache,
            )
        }))
        .buffer_unordered(build_jobs); // Use same parallelism as final builds

        while let Some((attr, intermediate_name, success, logs)) = stream.next().await {
            // Log is already streamed to file by build_intermediate_async

            // Check for false positive if build failed and flag is set
            let mut is_fp = false;
            if !success && false_positive {
                // Ensure base worktree exists (create lazily if needed from cached eval)
                let needs_worktree = !*base_worktree_exists.lock().unwrap();
                if needs_worktree {
                    info!(
                        "Creating base worktree for false positive check at {:?}",
                        base_path
                    );
                    if let Err(e) = run_command_async(
                        "git",
                        &[
                            "-C",
                            nixpkgs.to_str().unwrap(),
                            "worktree",
                            "add",
                            base_path.to_str().unwrap(),
                            &merge_base,
                        ],
                    )
                    .await
                    {
                        warn!("Failed to create base worktree: {}", e);
                    } else {
                        *base_worktree_exists.lock().unwrap() = true;
                    }
                }

                // Build same intermediate against base
                if *base_worktree_exists.lock().unwrap() {
                    info!(
                        "Checking if {}.{} is a false positive (building against base)...",
                        attr, intermediate_name
                    );
                    let (_, _, base_success, base_logs) = build_intermediate_async(
                        base_path.clone(),
                        attr.clone(),
                        intermediate_name.clone(),
                        system.to_string(),
                        0, // pr_num=0 to avoid log collision
                        true,
                    )
                    .await;

                    // Save base build log with commit suffix
                    let merge_base_short = &merge_base[..8.min(merge_base.len())];
                    if let Err(e) = save_single_log(
                        pr_num,
                        &attr,
                        &intermediate_name,
                        &base_logs,
                        Some(merge_base_short),
                    ) {
                        warn!("Failed to save base build log: {}", e);
                    }

                    // If base also fails, this is a false positive
                    if !base_success {
                        is_fp = true;
                        info!(
                            "{}.{} is a FALSE POSITIVE (also fails on base)",
                            attr, intermediate_name
                        );
                    } else {
                        info!(
                            "{}.{} is a REAL FAILURE (passes on base)",
                            attr, intermediate_name
                        );
                    }
                }
            }

            // Update results
            if let Some(result) = results.get_mut(&attr) {
                result.intermediate_results.push((
                    intermediate_name.clone(),
                    success,
                    logs.clone(),
                ));
                // If this is a false positive, mark the package
                if is_fp {
                    result.is_false_positive = true;
                }
            }

            // Update and save state
            if let Ok(mut state) = run_state.lock() {
                state
                    .intermediate_results
                    .entry(attr.clone())
                    .or_insert_with(Vec::new)
                    .push((intermediate_name.clone(), success, logs));

                if let Err(e) = save_run_state(&state) {
                    warn!("Failed to save run state: {}", e);
                }
            }

            info!(
                "Completed {}.{}: {}{}",
                attr,
                intermediate_name,
                if success { "SUCCESS" } else { "FAILED" },
                if is_fp { " (false positive)" } else { "" }
            );
        }
    }

    // Post intermediate results to GitHub (if not already posted)
    if !intermediates_already_posted && !dry_run {
        if let Some(token_str) = token {
            let intermediate_summary = {
                let state = run_state.lock().unwrap();
                build_intermediate_summary(pr_num, &state.intermediate_results)
            };
            if let Err(e) = post_github_comment(pr_num, token_str, &intermediate_summary).await {
                warn!("Failed to post intermediate results: {}", e);
            } else {
                info!("Posted intermediate results to PR #{}", pr_num);
            }
            // Mark as posted
            if let Ok(mut state) = run_state.lock() {
                state.intermediates_posted = true;
                let _ = save_run_state(&state);
            }
        }
    }

    // Build final packages (only if all intermediates succeeded)
    // Process one at a time to save state after each completion
    let packages_to_build: Vec<_> = results
        .iter()
        .filter(|(_, r)| {
            r.intermediate_results
                .iter()
                .all(|(_, success, _)| *success)
        })
        .map(|(attr, _)| attr.clone())
        .collect();

    if !packages_to_build.is_empty() {
        info!(
            "Building {} final packages ({} at a time)...",
            packages_to_build.len(),
            build_jobs
        );

        use futures::stream::{self, StreamExt};

        // Use buffer_unordered but process results as they complete
        let mut stream = stream::iter(packages_to_build.iter().map(|attr| {
            build_package_async(pr_path.clone(), attr.clone(), system.to_string(), pr_num)
        }))
        .buffer_unordered(build_jobs);

        while let Some((attr, success, logs)) = stream.next().await {
            // Log is already streamed to file by build_package_async

            // Update result
            if let Some(result) = results.get_mut(&attr) {
                result.package_success = success;
                result.package_logs = logs;

                // Add to completed results and save state
                let completed_result = result.clone();
                if let Ok(mut state) = run_state.lock() {
                    state.completed_results.push(completed_result);
                    // Save state after each package completes
                    if let Err(e) = save_run_state(&state) {
                        warn!("Failed to save run state: {}", e);
                    }
                }

                info!(
                    "Completed {}: {}",
                    attr,
                    if success { "SUCCESS" } else { "FAILED" }
                );
            }
        }
    }

    // Add any packages with failed intermediates to completed_results (they won't have been built)
    for (attr, result) in results.iter() {
        let has_failed_intermediate = result
            .intermediate_results
            .iter()
            .any(|(_, success, _)| !success);
        if has_failed_intermediate {
            // Check if already in completed_results
            let already_added = {
                let state = run_state.lock().unwrap();
                state.completed_results.iter().any(|r| r.attr == *attr)
            };
            if !already_added {
                if let Ok(mut state) = run_state.lock() {
                    state.completed_results.push(result.clone());
                    let _ = save_run_state(&state);
                }
            }
        }
    }

    // Cleanup PR worktree
    let _ = run_command_async(
        "git",
        &[
            "-C",
            nixpkgs.to_str().unwrap(),
            "worktree",
            "remove",
            "-f",
            pr_path.to_str().unwrap(),
        ],
    )
    .await;

    // Cleanup base worktree if it was created for false positive checks
    if *base_worktree_exists.lock().unwrap() {
        let _ = run_command_async(
            "git",
            &[
                "-C",
                nixpkgs.to_str().unwrap(),
                "worktree",
                "remove",
                "-f",
                base_path.to_str().unwrap(),
            ],
        )
        .await;
    }

    // Combine previously completed results with new results
    let all_results: Vec<_> = {
        let state = run_state.lock().unwrap();
        state.completed_results.clone()
    };

    let all_success = all_results.iter().all(|r| r.package_success);

    // Always persist logs locally
    let log_dir = save_logs_locally(pr_num, &all_results)?;
    info!("Logs saved to: {:?}", log_dir);

    // Delete state file since we completed successfully
    if let Err(e) = delete_run_state(pr_num) {
        warn!("Failed to delete run state: {}", e);
    }

    // Build the summary for posting to PR
    let summary = build_summary_comment(pr_num, &all_results);

    if !dry_run {
        if post_gist {
            // Post with gist containing logs
            if let Some(token_str) = token {
                create_gist_and_comment(pr_num, &all_results, token_str, dry_run).await?;
            } else {
                warn!("No GitHub token provided, skipping gist/comment");
            }
        } else {
            // Post summary directly to PR (no gist)
            if let Some(token_str) = token {
                post_github_comment(pr_num, token_str, &summary).await?;
                info!("Posted summary to PR #{}", pr_num);
            } else {
                warn!("No GitHub token provided, skipping comment");
            }
        }
    } else {
        info!("Dry run - would post comment:\n{}", summary);
    }

    // Print summary to stdout
    let passed = all_results.iter().filter(|r| r.package_success).count();
    let failed = all_results.len() - passed;
    info!(
        "Summary: {}/{} packages passed, {} failed",
        passed,
        all_results.len(),
        failed
    );
    for result in &all_results {
        if !result.package_success {
            info!("  FAILED: {}", result.attr);
        }
    }

    Ok(all_success)
}
