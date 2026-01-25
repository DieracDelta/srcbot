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
use crate::nix::{build_intermediate_async, build_package_async, pkg_attr_exists, run_nix_eval_jobs};
use crate::summary::build_summary_comment;
use crate::types::{
    ChangedPackage, ChangedPackageSer, FullEvalBuildResult, RemoteBuilderConfig, RunState,
    INTERMEDIATE_ATTRS,
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

/// Parse a disk size threshold like "50G", "100GB", "1T" into bytes
fn parse_size_threshold(threshold: &str) -> Option<u64> {
    let threshold = threshold.trim().to_uppercase();
    let (num_str, unit) = if threshold.ends_with("GB") {
        (&threshold[..threshold.len() - 2], "G")
    } else if threshold.ends_with('G') {
        (&threshold[..threshold.len() - 1], "G")
    } else if threshold.ends_with("TB") {
        (&threshold[..threshold.len() - 2], "T")
    } else if threshold.ends_with('T') {
        (&threshold[..threshold.len() - 1], "T")
    } else if threshold.ends_with("MB") {
        (&threshold[..threshold.len() - 2], "M")
    } else if threshold.ends_with('M') {
        (&threshold[..threshold.len() - 1], "M")
    } else {
        return None;
    };

    let num: u64 = num_str.parse().ok()?;
    match unit {
        "G" => Some(num * 1024 * 1024 * 1024),
        "T" => Some(num * 1024 * 1024 * 1024 * 1024),
        "M" => Some(num * 1024 * 1024),
        _ => None,
    }
}

/// Check disk usage on remote and run GC if above threshold
async fn maybe_run_remote_gc(config: &RemoteBuilderConfig) -> Result<()> {
    let threshold = match &config.gc_threshold {
        Some(t) => t,
        None => return Ok(()), // No threshold configured
    };

    let threshold_bytes = match parse_size_threshold(threshold) {
        Some(b) => b,
        None => {
            warn!(
                "Invalid GC threshold '{}', expected format like '50G' or '100GB'",
                threshold
            );
            return Ok(());
        }
    };

    info!(
        "Checking disk usage on remote {} (threshold: {})",
        config.ssh_target, threshold
    );

    // Get disk usage via SSH using df
    // df -B1 gives output in bytes
    let output = run_command_async(
        "ssh",
        &[&config.ssh_target, "df", "-B1", "/nix/store"],
    )
    .await?;

    // Parse df output:
    // Filesystem     1B-blocks         Used    Available Use% Mounted on
    // /dev/sda1      107374182400  53687091200  53687091200  50% /nix/store
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() < 2 {
        warn!("Could not parse df output from remote");
        return Ok(());
    }

    let parts: Vec<&str> = lines[1].split_whitespace().collect();
    if parts.len() < 3 {
        warn!("Could not parse df output from remote: {}", lines[1]);
        return Ok(());
    }

    let used_bytes: u64 = match parts[2].parse() {
        Ok(b) => b,
        Err(_) => {
            warn!("Could not parse used bytes from df output: {}", parts[2]);
            return Ok(());
        }
    };

    let used_gb = used_bytes / (1024 * 1024 * 1024);
    let threshold_gb = threshold_bytes / (1024 * 1024 * 1024);

    info!(
        "Remote /nix/store usage: {}GB (threshold: {}GB)",
        used_gb, threshold_gb
    );

    if used_bytes > threshold_bytes {
        info!(
            "Disk usage exceeds threshold, running garbage collection on {}...",
            config.ssh_target
        );

        let gc_cmd = if let Some(days) = config.gc_keep_days {
            format!("nix-collect-garbage --delete-older-than {}d", days)
        } else {
            "nix-collect-garbage".to_string()
        };

        match run_command_async("ssh", &[&config.ssh_target, &gc_cmd]).await {
            Ok(output) => {
                info!("Remote GC completed: {}", output.lines().next().unwrap_or("done"));
            }
            Err(e) => {
                warn!("Remote GC failed: {}", e);
            }
        }
    } else {
        info!("Disk usage within threshold, no GC needed");
    }

    Ok(())
}

/// Build all packages for a single system
/// Returns the build results for this system
#[allow(clippy::too_many_arguments)]
async fn build_packages_for_system(
    system: String,
    packages: Vec<ChangedPackage>,
    pr_path: PathBuf,
    base_path: PathBuf,
    nixpkgs: PathBuf,
    base_commit: String,
    builders_str: Option<String>,
    build_jobs: usize,
    pr_num: u64,
    nix_timeout: u64,
    full_rebuild: bool,
    false_positive: bool,
    base_worktree_exists: Arc<Mutex<bool>>,
    saved_intermediate_results: HashMap<String, Vec<(String, bool, String)>>,
) -> Result<Vec<FullEvalBuildResult>> {
    use futures::stream::{self, StreamExt};

    if packages.is_empty() {
        info!("No packages to build for {}", system);
        return Ok(Vec::new());
    }

    info!(
        "Building {} packages for {} (build_jobs={})",
        packages.len(),
        system,
        build_jobs
    );

    if let Some(ref b) = builders_str {
        info!("[{}] Using remote builder: {}", system, b);
    }

    // Initialize results for this system
    let mut results: HashMap<String, FullEvalBuildResult> = packages
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
                    system: system.clone(),
                    intermediate_results: saved_intermediates,
                    package_success: false,
                    package_logs: String::new(),
                    is_false_positive: false,
                    is_non_deterministic: false,
                    exists_on_base: true, // Will be set to false if detected as new package
                },
            )
        })
        .collect();

    // Build intermediates tier by tier
    for intermediate in INTERMEDIATE_ATTRS {
        let packages_needing: Vec<_> = packages
            .iter()
            .filter(|p| {
                if !p.changed_intermediates.contains(&intermediate.to_string()) {
                    return false;
                }
                if let Some(result) = results.get(&p.attr) {
                    if result
                        .intermediate_results
                        .iter()
                        .any(|(name, _, _)| name == intermediate)
                    {
                        return false;
                    }
                }
                true
            })
            .collect();

        if packages_needing.is_empty() {
            continue;
        }

        info!(
            "[{}] Building {} packages' .{} attribute...",
            system,
            packages_needing.len(),
            intermediate
        );

        let use_cache = !full_rebuild;
        // Collect the attrs we need to build (owned Strings, not references)
        let attrs_to_build: Vec<String> =
            packages_needing.iter().map(|p| p.attr.clone()).collect();

        let mut stream = stream::iter(attrs_to_build.into_iter().map(|attr| {
            let builders_clone = builders_str.clone();
            let pr_path_inner = pr_path.clone();
            let sys_inner = system.clone();
            async move {
                build_intermediate_async(
                    pr_path_inner,
                    attr,
                    intermediate.to_string(),
                    sys_inner,
                    pr_num,
                    use_cache,
                    nix_timeout,
                    builders_clone.as_deref(),
                )
                .await
            }
        }))
        .buffer_unordered(build_jobs);

        while let Some((attr, intermediate_name, success, logs, is_non_det)) = stream.next().await {
            if is_non_det {
                if let Some(result) = results.get_mut(&attr) {
                    result.is_non_deterministic = true;
                }
            }

            // Check for false positive if build failed
            let mut is_fp = false;
            if !success && false_positive {
                // Try to create base worktree if needed (with synchronization)
                let needs_worktree = {
                    let exists = base_worktree_exists.lock().unwrap();
                    !*exists
                };

                if needs_worktree {
                    info!(
                        "[{}] Creating base worktree for false positive check",
                        system
                    );
                    // Use a separate lock scope to avoid holding lock during await
                    let should_create = {
                        let mut exists = base_worktree_exists.lock().unwrap();
                        if !*exists {
                            *exists = true; // Claim the creation
                            true
                        } else {
                            false
                        }
                    };

                    if should_create {
                        if let Err(e) = run_command_async(
                            "git",
                            &[
                                "-C",
                                nixpkgs.to_str().unwrap(),
                                "worktree",
                                "add",
                                base_path.to_str().unwrap(),
                                &base_commit,
                            ],
                        )
                        .await
                        {
                            warn!("Failed to create base worktree: {}", e);
                            // Reset the flag since creation failed
                            *base_worktree_exists.lock().unwrap() = false;
                        }
                    }
                }

                if *base_worktree_exists.lock().unwrap() {
                    // First check if the attribute exists on the base branch
                    let attr_exists_on_base =
                        pkg_attr_exists(&base_path, &attr, &system).await;

                    if !attr_exists_on_base {
                        info!(
                            "[{}] {}.{} is a NEW package (doesn't exist on base), not a false positive",
                            system, attr, intermediate_name
                        );
                        // Mark as new package so summary doesn't show "before" links
                        if let Some(result) = results.get_mut(&attr) {
                            result.exists_on_base = false;
                        }
                    } else {
                        info!(
                            "[{}] Checking if {}.{} is a false positive...",
                            system, attr, intermediate_name
                        );
                        let (_, _, base_success, base_logs, _) = build_intermediate_async(
                            base_path.clone(),
                            attr.clone(),
                            intermediate_name.clone(),
                            system.clone(),
                            0,
                            true,
                            nix_timeout,
                            None, // Base builds always local
                        )
                        .await;

                        let base_commit_short = &base_commit[..8.min(base_commit.len())];
                        if let Err(e) = save_single_log(
                            pr_num,
                            &attr,
                            &intermediate_name,
                            &base_logs,
                            Some(base_commit_short),
                            Some(&system),
                        ) {
                            warn!("Failed to save base build log: {}", e);
                        }

                        if !base_success {
                            is_fp = true;
                            info!(
                                "[{}] {}.{} is a FALSE POSITIVE (also fails on base)",
                                system, attr, intermediate_name
                            );
                        }
                    }
                }
            }

            if let Some(result) = results.get_mut(&attr) {
                result
                    .intermediate_results
                    .push((intermediate_name.clone(), success, logs.clone()));
                if is_fp {
                    result.is_false_positive = true;
                }
            }

            info!(
                "[{}] Completed {}.{}: {}{}",
                system,
                attr,
                intermediate_name,
                if success { "SUCCESS" } else { "FAILED" },
                if is_fp { " (false positive)" } else { "" }
            );
        }
    }

    // Build final packages
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
            "[{}] Building {} final packages...",
            system,
            packages_to_build.len()
        );

        let mut stream = stream::iter(packages_to_build.into_iter().map(|attr| {
            let pr_path_inner = pr_path.clone();
            let sys_inner = system.clone();
            let builders_clone = builders_str.clone();
            async move {
                build_package_async(
                    pr_path_inner,
                    attr,
                    sys_inner,
                    pr_num,
                    builders_clone.as_deref(),
                )
                .await
            }
        }))
        .buffer_unordered(build_jobs);

        while let Some((attr, success, logs)) = stream.next().await {
            if let Some(result) = results.get_mut(&attr) {
                result.package_success = success;
                result.package_logs = logs.clone();

                // False positive check for package failures
                if !success && false_positive && *base_worktree_exists.lock().unwrap() {
                    // First check if the attribute exists on the base branch
                    let attr_exists_on_base =
                        pkg_attr_exists(&base_path, &attr, &system).await;

                    if !attr_exists_on_base {
                        info!(
                            "[{}] {} is a NEW package (doesn't exist on base), not a false positive",
                            system, attr
                        );
                        // Mark as new package so summary doesn't show "before" links
                        result.exists_on_base = false;
                    } else {
                        info!(
                            "[{}] Checking if {} package is a false positive...",
                            system, attr
                        );

                        let (_, base_success, base_logs) = build_package_async(
                            base_path.clone(),
                            attr.clone(),
                            system.clone(),
                            0,
                            None,
                        )
                        .await;

                        let base_commit_short = &base_commit[..8.min(base_commit.len())];
                        if let Err(e) = save_single_log(
                            pr_num,
                            &attr,
                            "package",
                            &base_logs,
                            Some(base_commit_short),
                            Some(&system),
                        ) {
                            warn!("Failed to save base package log: {}", e);
                        }

                        if !base_success {
                            result.is_false_positive = true;
                            info!(
                                "[{}] {} package is a FALSE POSITIVE (also fails on base)",
                                system, attr
                            );
                        }
                    }
                }

                info!(
                    "[{}] Completed {}: {}{}",
                    system,
                    attr,
                    if success { "SUCCESS" } else { "FAILED" },
                    if result.is_false_positive {
                        " (false positive)"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    // Collect all results
    let all_results: Vec<FullEvalBuildResult> = results.into_values().collect();
    Ok(all_results)
}

/// Process a PR in full-eval mode
///
/// If `full_rebuild` is true, uses the old behavior (prune store paths, rebuild with --substituters "").
/// If `full_rebuild` is false (default), uses cache-friendly mode (allow cache, verify with --check).
/// If `false_positive` is true, when a build fails, check if it also fails on the base branch.
/// If `verify_full_drvs` is true, also detect packages where the final drvPath changed (not just intermediates).
/// If `log_base_url` is provided, log URLs will be included in the summary.
/// If `remote_config` is provided, builds for that system are routed to the remote builder.
/// `cli_command` is the command that was run, for inclusion in the summary.
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
    base_commit_override: Option<&str>,
    verify_full_drvs: bool,
    nix_timeout: u64,
    log_base_url: Option<&str>,
    remote_config: Option<&RemoteBuilderConfig>,
    cli_command: &str,
) -> Result<bool> {
    info!("srcbot: Full evaluation mode for PR #{}", pr_num);

    // Determine all systems to test
    let local_system = system.to_string();
    let mut systems_to_test = vec![local_system.clone()];
    if let Some(rc) = remote_config {
        if rc.system != local_system {
            systems_to_test.push(rc.system.clone());
            info!(
                "Multi-arch mode: testing {} (local) and {} (remote: {})",
                local_system, rc.system, rc.ssh_target
            );
        }
    }
    info!(
        "Testing {} system(s): {:?}",
        systems_to_test.len(),
        systems_to_test
    );

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

    // Use override if provided, otherwise use computed merge_base
    let base_commit = if let Some(override_commit) = base_commit_override {
        info!("Using override base commit: {}", override_commit);
        override_commit.to_string()
    } else {
        merge_base.clone()
    };

    // Check if saved state is valid (matches current PR state)
    // Note: Resume is only supported for single-system runs
    let valid_saved_state = if systems_to_test.len() > 1 {
        if saved_state.is_some() {
            warn!("Resume not supported for multi-arch runs, starting fresh");
        }
        None
    } else {
        saved_state.as_ref().and_then(|state| {
            if state.merge_base == merge_base
                && state.pr_head_sha == *pr_head_sha
                && state.system == local_system
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
        })
    };

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
    // For multi-arch: all_changed maps system -> changed packages
    let (
        all_changed,
        completed_results,
        saved_intermediate_results,
        _intermediates_already_posted,
    ): (
        HashMap<String, Vec<ChangedPackage>>,
        Vec<FullEvalBuildResult>,
        HashMap<String, Vec<(String, bool, String)>>,
        bool,
    ) = if let Some(state) = valid_saved_state {
        // Convert saved state back to working format (single-system resume)
        let changed: Vec<ChangedPackage> = state
            .packages_to_build
            .iter()
            .map(|p| ChangedPackage {
                attr: p.attr.clone(),
                changed_intermediates: p.changed_intermediates.clone(),
                intermediate_drv_paths: HashMap::new(),
                final_drv_changed: p.final_drv_changed,
            })
            .collect();
        let mut all_changed = HashMap::new();
        all_changed.insert(local_system.clone(), changed);
        (
            all_changed,
            state.completed_results.clone(),
            state.intermediate_results.clone(),
            state.intermediates_posted,
        )
    } else {
        // Fresh evaluation for all systems
        let mut all_changed: HashMap<String, Vec<ChangedPackage>> = HashMap::new();
        let mut base_worktree_created = false;

        for sys in &systems_to_test {
            info!("=== Evaluating system: {} ===", sys);

            // Try to load cached base eval results, otherwise evaluate
            let base_packages = if let Some(cached) = load_cached_eval(&base_commit, sys) {
                info!("Using cached base eval for {} ({} packages)", sys, cached.len());
                cached
            } else {
                // Create base worktree if not already created
                if !base_worktree_created {
                    info!("Creating base worktree ({}) at {:?}", base_commit, base_path);
                    run_command_async(
                        "git",
                        &[
                            "-C",
                            nixpkgs.to_str().unwrap(),
                            "worktree",
                            "add",
                            base_path.to_str().unwrap(),
                            &base_commit,
                        ],
                    )
                    .await?;
                    base_worktree_created = true;
                    *base_worktree_exists.lock().unwrap() = true;
                }

                info!("Evaluating base nixpkgs for {}...", sys);
                let packages = run_nix_eval_jobs(&base_path, sys, eval_workers).await?;

                // Cache the results
                if let Err(e) = save_eval_cache(&base_commit, sys, &packages) {
                    warn!("Failed to save eval cache for {}: {}", sys, e);
                }

                packages
            };

            // Try to load cached PR eval results, otherwise evaluate
            let pr_packages = if let Some(cached) = load_cached_eval(pr_head_sha, sys) {
                info!("Using cached PR eval for {} ({} packages)", sys, cached.len());
                cached
            } else {
                info!("Evaluating PR nixpkgs for {}...", sys);
                let packages = run_nix_eval_jobs(&pr_path, sys, eval_workers).await?;

                // Cache the results
                if let Err(e) = save_eval_cache(pr_head_sha, sys, &packages) {
                    warn!("Failed to save PR eval cache for {}: {}", sys, e);
                }

                packages
            };

            // Find changed packages for this system
            let changed = find_changed_packages(&base_packages, &pr_packages, verify_full_drvs);
            info!("Found {} changed packages for {}", changed.len(), sys);
            all_changed.insert(sys.clone(), changed);
        }

        // Cleanup base worktree if we don't need it for false positive checks
        if base_worktree_created && !false_positive {
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

        // Create base worktree for false positive checks if needed and not already created
        // (eval cache is separate from needing the worktree for false positive builds)
        if false_positive && !base_worktree_created && all_changed.values().any(|v| !v.is_empty()) {
            info!(
                "Creating base worktree for false positive checks ({}) at {:?}",
                base_commit, base_path
            );
            if let Err(e) = run_command_async(
                "git",
                &[
                    "-C",
                    nixpkgs.to_str().unwrap(),
                    "worktree",
                    "add",
                    base_path.to_str().unwrap(),
                    &base_commit,
                ],
            )
            .await
            {
                warn!("Failed to create base worktree for false positive checks: {}", e);
            } else {
                *base_worktree_exists.lock().unwrap() = true;
            }
        }

        (all_changed, Vec::new(), HashMap::new(), false)
    };

    // Check if any system has changed packages
    let total_changed: usize = all_changed.values().map(|v| v.len()).sum();
    if total_changed == 0 {
        let msg = if verify_full_drvs {
            "No packages with changed intermediates or drvPaths found!"
        } else {
            "No packages with changed intermediates found!"
        };
        info!("{}", msg);

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

        if !dry_run {
            if let Some(token_str) = token {
                let comment_msg = if verify_full_drvs {
                    "## srcbot: Full Evaluation Results\n\nNo packages with changed intermediates or drvPaths detected."
                } else {
                    "## srcbot: Full Evaluation Results\n\nNo packages with changed source intermediates detected."
                };
                let comment_url = post_github_comment(pr_num, token_str, comment_msg).await?;
                println!("\nComment posted: {}", comment_url);
            }
        }
        return Ok(true);
    }

    // Log changed packages per system
    for (sys, changed) in &all_changed {
        if !changed.is_empty() {
            info!("Changed packages for {}:", sys);
            for pkg in changed {
                if pkg.final_drv_changed {
                    info!("  - {}: [final drvPath changed]", pkg.attr);
                } else {
                    info!("  - {}: {:?}", pkg.attr, pkg.changed_intermediates);
                }
            }
        }
    }

    // For single-system resume support, get the changed packages for local system
    let changed_for_state: Vec<ChangedPackage> = all_changed
        .get(&local_system)
        .cloned()
        .unwrap_or_default();

    // Get set of already completed package attrs (for resume)
    let completed_attrs: std::collections::HashSet<String> =
        completed_results.iter().map(|r| r.attr.clone()).collect();

    // Note: For multi-arch, we don't support resume, so completed_results will be empty
    // and completed_attrs will be empty, meaning all packages will be built

    // Build packages for all systems IN PARALLEL
    let mut all_results: Vec<FullEvalBuildResult> = completed_results;

    // Run remote GC if threshold is configured
    if let Some(rc) = remote_config {
        if let Err(e) = maybe_run_remote_gc(rc).await {
            warn!("Remote GC check failed: {}", e);
        }
    }

    // Prepare build tasks for each system
    let mut build_handles: Vec<tokio::task::JoinHandle<Result<Vec<FullEvalBuildResult>>>> =
        Vec::new();

    for sys in &systems_to_test {
        let changed = match all_changed.get(sys) {
            Some(c) if !c.is_empty() => c.clone(),
            _ => {
                info!("No changed packages for {}, skipping", sys);
                continue;
            }
        };

        // Filter to packages not yet completed (for resume support)
        let remaining_packages: Vec<ChangedPackage> = changed
            .into_iter()
            .filter(|p| !completed_attrs.contains(&p.attr))
            .collect();

        if remaining_packages.is_empty() {
            info!("All packages already completed for {}", sys);
            continue;
        }

        // Determine builders and build_jobs for this system
        let (builders_str, system_build_jobs): (Option<String>, usize) = if *sys == local_system {
            (None, build_jobs) // Local builds use --build-jobs
        } else {
            // Remote builds use --remote-build-jobs (from remote_config.max_jobs)
            let builders = remote_config.map(|rc| rc.to_builders_arg());
            let jobs = remote_config.map(|rc| rc.max_jobs).unwrap_or(build_jobs);
            (builders, jobs)
        };

        info!("=========================================");
        info!(
            "Starting builds for system: {} ({} packages, {} jobs{})",
            sys,
            remaining_packages.len(),
            system_build_jobs,
            if builders_str.is_some() {
                ", remote"
            } else {
                ", local"
            }
        );
        info!("=========================================");

        // Clone values needed by the spawned task
        let sys_clone = sys.clone();
        let pr_path_clone = pr_path.clone();
        let base_path_clone = base_path.clone();
        let nixpkgs_clone = nixpkgs.clone();
        let base_commit_clone = base_commit.clone();
        let base_worktree_exists_clone = base_worktree_exists.clone();
        let saved_intermediate_results_clone = saved_intermediate_results.clone();

        let handle = tokio::spawn(async move {
            build_packages_for_system(
                sys_clone,
                remaining_packages,
                pr_path_clone,
                base_path_clone,
                nixpkgs_clone,
                base_commit_clone,
                builders_str,
                system_build_jobs,
                pr_num,
                nix_timeout,
                full_rebuild,
                false_positive,
                base_worktree_exists_clone,
                saved_intermediate_results_clone,
            )
            .await
        });

        build_handles.push(handle);
    }

    // Wait for all builds to complete and collect results
    info!(
        "Waiting for {} parallel build task(s) to complete...",
        build_handles.len()
    );
    for handle in build_handles {
        match handle.await {
            Ok(Ok(system_results)) => {
                all_results.extend(system_results);
            }
            Ok(Err(e)) => {
                warn!("Build task failed: {}", e);
            }
            Err(e) => {
                warn!("Build task panicked: {}", e);
            }
        }
    }

    // Create run state for single-system resume (not used for multi-arch)
    let run_state = Arc::new(Mutex::new(RunState {
        pr_num,
        merge_base: merge_base.clone(),
        pr_head_sha: pr_head_sha.clone(),
        system: local_system.clone(),
        cli_command: cli_command.to_string(),
        packages_to_build: changed_for_state
            .iter()
            .map(|p| ChangedPackageSer {
                attr: p.attr.clone(),
                changed_intermediates: p.changed_intermediates.clone(),
                final_drv_changed: p.final_drv_changed,
            })
            .collect(),
        intermediate_results: HashMap::new(),
        completed_results: all_results
            .iter()
            .filter(|r| r.system == local_system)
            .cloned()
            .collect(),
        intermediates_posted: false,
    }));

    // Set up Ctrl-C handler
    let state_for_handler = run_state.clone();
    let _handler_pr_num = pr_num;
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            eprintln!("\nInterrupted! Saving state...");
            if let Ok(state) = state_for_handler.lock() {
                if let Err(e) = save_run_state(&state) {
                    eprintln!("Failed to save state: {}", e);
                } else {
                    eprintln!("State saved. Resume with --resume flag (single-system only).");
                }
            }
            std::process::exit(130);
        }
    });

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

    let all_success = all_results.iter().all(|r| r.package_success);

    // Always persist logs locally
    let log_dir = save_logs_locally(pr_num, &all_results)?;
    info!("Logs saved to: {:?}", log_dir);

    // Delete state file since we completed successfully
    if let Err(e) = delete_run_state(pr_num) {
        warn!("Failed to delete run state: {}", e);
    }

    // Build log URL base if log_base_url is provided
    let log_url_base = log_base_url.map(|base| {
        let log_dir_name = log_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        format!("{}/logs/{}", base, log_dir_name)
    });

    // Build the summary for posting to PR
    let base_commit_short = &base_commit[..8.min(base_commit.len())];
    let summary = build_summary_comment(
        pr_num,
        &all_results,
        log_url_base.as_deref(),
        Some(&base_commit),
        Some(pr_head_sha),
        Some(base_commit_short),
        Some(cli_command),
        remote_config,
    );

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
                let comment_url = post_github_comment(pr_num, token_str, &summary).await?;
                println!("\nComment posted: {}", comment_url);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size_threshold_gigabytes() {
        assert_eq!(
            parse_size_threshold("50G"),
            Some(50 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            parse_size_threshold("50GB"),
            Some(50 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            parse_size_threshold("100g"),
            Some(100 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            parse_size_threshold("100gb"),
            Some(100 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn test_parse_size_threshold_terabytes() {
        assert_eq!(
            parse_size_threshold("1T"),
            Some(1 * 1024 * 1024 * 1024 * 1024)
        );
        assert_eq!(
            parse_size_threshold("2TB"),
            Some(2 * 1024 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn test_parse_size_threshold_megabytes() {
        assert_eq!(
            parse_size_threshold("500M"),
            Some(500 * 1024 * 1024)
        );
        assert_eq!(
            parse_size_threshold("500MB"),
            Some(500 * 1024 * 1024)
        );
    }

    #[test]
    fn test_parse_size_threshold_invalid() {
        assert_eq!(parse_size_threshold("50"), None);
        assert_eq!(parse_size_threshold("abc"), None);
        assert_eq!(parse_size_threshold("50K"), None); // KB not supported
        assert_eq!(parse_size_threshold(""), None);
    }

    #[test]
    fn test_parse_size_threshold_with_whitespace() {
        assert_eq!(
            parse_size_threshold("  50G  "),
            Some(50 * 1024 * 1024 * 1024)
        );
    }
}
