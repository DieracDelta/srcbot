use crate::types::{FullEvalBuildResult, RemoteBuilderConfig};
use std::collections::HashMap;

/// Format a CLI command with line breaks for readability
fn format_cli_command(cmd: &str) -> String {
    // Split on " --" to get each flag
    let parts: Vec<&str> = cmd.split(" --").collect();
    if parts.len() <= 1 {
        return cmd.to_string();
    }

    // First part is the command itself (e.g., "srcbot verify")
    let mut formatted = parts[0].to_string();

    // Add each flag on its own line with backslash continuation
    for part in &parts[1..] {
        formatted.push_str(" \\\n  --");
        formatted.push_str(part);
    }

    formatted
}

/// Build a log URL for an attribute and step, including system prefix
fn build_log_url(log_url_base: &str, system: &str, attr: &str, step: &str) -> String {
    let attr_safe = attr.replace('.', "_").replace('/', "_");
    format!("{}/{}.{}.{}.log", log_url_base, system, attr_safe, step)
}

/// Build a base (before) log URL for an attribute and step, including system prefix
fn build_base_log_url(
    log_url_base: &str,
    system: &str,
    attr: &str,
    step: &str,
    base_commit_short: &str,
) -> String {
    let attr_safe = attr.replace('.', "_").replace('/', "_");
    format!(
        "{}/{}.{}.{}.base-{}.log",
        log_url_base, system, attr_safe, step, base_commit_short
    )
}

/// Build a formatted string of steps with links
/// For failures (include_before_after=true):
///   - Shows all steps with success/fail markers
///   - If exists_on_base=true: `[src](...)` (passed), `package ([before](...), [after](...))` (failed)
///   - If exists_on_base=false (new package): `[src](...)` (passed), `[package](...)` (failed)
/// For passed (include_before_after=false): `[src](...), [package](...)`
fn build_steps_with_links(
    result: &FullEvalBuildResult,
    log_url_base: Option<&str>,
    base_commit_short: Option<&str>,
    include_before_after: bool,
) -> String {
    // Collect steps with their success status
    let mut steps: Vec<(&str, bool)> = result
        .intermediate_results
        .iter()
        .map(|(name, success, _)| (name.as_str(), *success))
        .collect();
    steps.push(("package", result.package_success));

    // Use result's system for log URLs, default to x86_64-linux if not set
    let system = if result.system.is_empty() {
        "x86_64-linux"
    } else {
        &result.system
    };

    // Only show "before" links if the package exists on base (not a new package)
    let show_before = include_before_after && result.exists_on_base;

    match (log_url_base, base_commit_short, show_before) {
        (Some(base), Some(commit_short), true) if include_before_after => {
            // Failed package with base comparison available
            // Show before/after for failed steps, label for passed steps
            steps
                .iter()
                .map(|(step, success)| {
                    let after_url = build_log_url(base, system, &result.attr, step);
                    if *success {
                        // Passed step - labeled with simple link
                        format!("{} (passed): [log]({})", step, after_url)
                    } else {
                        // Failed step - before/after links
                        let before_url = build_base_log_url(base, system, &result.attr, step, commit_short);
                        format!("{} (failed): [before]({}), [after]({})", step, before_url, after_url)
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ")
        }
        (Some(base), _, _) if include_before_after => {
            // Failed package but no base comparison (new package)
            steps
                .iter()
                .map(|(step, success)| {
                    let url = build_log_url(base, system, &result.attr, step);
                    if *success {
                        format!("{} (passed): [log]({})", step, url)
                    } else {
                        format!("{} (failed): [log]({})", step, url)
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ")
        }
        (Some(base), _, _) => {
            // Passed packages - simple links, no labels needed
            steps
                .iter()
                .map(|(step, _)| {
                    let url = build_log_url(base, system, &result.attr, step);
                    format!("[{}]({})", step, url)
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => {
            // No links - just step names
            steps
                .iter()
                .map(|(step, _)| *step)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// Build the summary comment to post to the PR
///
/// # Arguments
/// * `pr_num` - PR number
/// * `results` - Build results (may contain multiple systems)
/// * `log_url_base` - Optional base URL for log files
/// * `base_commit` - Optional base commit hash
/// * `head_commit` - Optional head commit hash (from the PR)
/// * `base_commit_short` - Optional short base commit hash for log URLs
/// * `cli_command` - Optional CLI command that was run (shown in collapsible section)
/// * `remote_config` - Optional remote builder config (for display in system headers)
pub fn build_summary_comment(
    pr_num: u64,
    results: &[FullEvalBuildResult],
    log_url_base: Option<&str>,
    base_commit: Option<&str>,
    head_commit: Option<&str>,
    base_commit_short: Option<&str>,
    cli_command: Option<&str>,
    _remote_config: Option<&RemoteBuilderConfig>,
) -> String {
    // Group results by system
    let mut results_by_system: HashMap<String, Vec<&FullEvalBuildResult>> = HashMap::new();
    for result in results {
        let system = if result.system.is_empty() {
            "x86_64-linux".to_string()
        } else {
            result.system.clone()
        };
        results_by_system
            .entry(system)
            .or_insert_with(Vec::new)
            .push(result);
    }

    // Get list of systems, sorted (local system first)
    let mut systems: Vec<String> = results_by_system.keys().cloned().collect();
    systems.sort_by(|a, b| {
        // Put x86_64-linux first (local), then others
        if a == "x86_64-linux" {
            std::cmp::Ordering::Less
        } else if b == "x86_64-linux" {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(b)
        }
    });

    let is_multi_arch = systems.len() > 1;

    // Calculate totals
    let total_passed: usize = results.iter().filter(|r| r.package_success).count();
    let total_failed: usize = results.iter().filter(|r| !r.package_success).count();
    let total_false_positives: usize = results
        .iter()
        .filter(|r| !r.package_success && r.is_false_positive)
        .count();
    let total_non_deterministic: usize = results.iter().filter(|r| r.is_non_deterministic).count();

    let mut summary = format!("## srcbot: Full Evaluation Results for PR #{}\n", pr_num);

    // Show base and head commits on a separate line if available
    match (base_commit, head_commit) {
        (Some(base), Some(head)) => {
            let base_short = &base[..8.min(base.len())];
            let head_short = &head[..8.min(head.len())];
            summary.push_str(&format!(
                "\ncompared base commit `{}` against head of PR commit `{}`\n",
                base_short, head_short
            ));
        }
        (Some(base), None) => {
            let base_short = &base[..8.min(base.len())];
            summary.push_str(&format!("\nbase commit: `{}`\n", base_short));
        }
        (None, Some(head)) => {
            let head_short = &head[..8.min(head.len())];
            summary.push_str(&format!("\nhead of PR commit: `{}`\n", head_short));
        }
        (None, None) => {}
    }

    // Add CLI command in collapsible section if provided
    if let Some(cmd) = cli_command {
        if !cmd.is_empty() {
            summary.push_str("\n<details>\n<summary>Command</summary>\n\n```bash\n");
            summary.push_str(&format_cli_command(cmd));
            summary.push_str("\n```\n</details>\n");
        }
    }

    // Overall status
    summary.push_str(&format!(
        "\n**Status**: {}/{} packages passed, {} failed",
        total_passed,
        results.len(),
        total_failed
    ));

    if total_false_positives > 0 {
        summary.push_str(&format!(" ({} pre-existing)", total_false_positives));
    }
    if total_non_deterministic > 0 {
        summary.push_str(&format!(", {} non-deterministic", total_non_deterministic));
    }
    if is_multi_arch {
        summary.push_str(&format!(" across {} architectures", systems.len()));
    }
    summary.push_str("\n\n");

    // Generate output per system
    for system in &systems {
        let system_results = match results_by_system.get(system) {
            Some(r) => r,
            None => continue,
        };

        // Add system header for multi-arch
        if is_multi_arch {
            summary.push_str(&format!("### {}\n\n", system));
        }

        let mut passed: Vec<_> = system_results
            .iter()
            .filter(|r| r.package_success)
            .collect();
        passed.sort_by(|a, b| a.attr.cmp(&b.attr));

        let mut real_failed: Vec<_> = system_results
            .iter()
            .filter(|r| !r.package_success && !r.is_false_positive)
            .collect();
        real_failed.sort_by(|a, b| a.attr.cmp(&b.attr));

        let mut false_positives: Vec<_> = system_results
            .iter()
            .filter(|r| !r.package_success && r.is_false_positive)
            .collect();
        false_positives.sort_by(|a, b| a.attr.cmp(&b.attr));

        // Show real failures first (introduced by this PR)
        if !real_failed.is_empty() {
            summary.push_str("<details>\n<summary>");
            summary.push_str(&format!(
                "Failed Packages (introduced by this PR) - {} packages</summary>\n\n",
                real_failed.len()
            ));

            summary.push_str("| Package | Steps |\n|---------|-------|\n");

            for result in &real_failed {
                let steps = build_steps_with_links(
                    result,
                    log_url_base,
                    base_commit_short,
                    true,
                );
                summary.push_str(&format!("| {} | {} |\n", result.attr, steps));
            }
            summary.push_str("\n</details>\n\n");
        }

        // Show false positives (pre-existing failures)
        if !false_positives.is_empty() {
            summary.push_str("<details>\n<summary>");
            summary.push_str(&format!(
                "Pre-existing Failures (false positives) - {} packages</summary>\n\n",
                false_positives.len()
            ));
            if let Some(commit) = base_commit {
                let short_commit = &commit[..8.min(commit.len())];
                summary.push_str(&format!(
                    "These packages also fail on the base branch (`{}`).\n\n",
                    short_commit
                ));
            } else {
                summary.push_str("These packages also fail on the base branch.\n\n");
            }

            summary.push_str("| Package | Steps |\n|---------|-------|\n");

            for result in &false_positives {
                let steps = build_steps_with_links(
                    result,
                    log_url_base,
                    base_commit_short,
                    true,
                );
                summary.push_str(&format!("| {} | {} |\n", result.attr, steps));
            }
            summary.push_str("\n</details>\n\n");
        }

        if !passed.is_empty() {
            summary.push_str("<details>\n<summary>");
            summary.push_str(&format!("{} packages passed</summary>\n\n", passed.len()));
            summary.push_str("| Package | Steps Built |\n|---------|-------------|\n");
            for result in &passed {
                let steps = build_steps_with_links(
                    result,
                    log_url_base,
                    base_commit_short,
                    false,
                );
                let non_det_marker = if result.is_non_deterministic {
                    " (non-deterministic)"
                } else {
                    ""
                };
                summary.push_str(&format!(
                    "| {}{} | {} |\n",
                    result.attr, non_det_marker, steps
                ));
            }
            summary.push_str("\n</details>\n\n");
        }
    }

    summary.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_summary_comment_all_passed() {
        let results = vec![FullEvalBuildResult {
            attr: "hello".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
            package_success: true,
            package_logs: "".to_string(),
            is_false_positive: false,
            is_non_deterministic: false,
            exists_on_base: true,
        }];
        let summary = build_summary_comment(123, &results, None, None, None, None, None, None);
        assert!(summary.contains("1/1 packages passed"));
        assert!(summary.contains("0 failed"));
        assert!(!summary.contains("pre-existing"));
    }

    #[test]
    fn test_build_summary_comment_real_failure() {
        let results = vec![FullEvalBuildResult {
            attr: "broken".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), false, "error".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: false,
            is_non_deterministic: false,
            exists_on_base: true,
        }];
        let summary = build_summary_comment(123, &results, None, None, None, None, None, None);
        assert!(summary.contains("0/1 packages passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("introduced by this PR"));
        assert!(summary.contains("broken"));
        assert!(!summary.contains("pre-existing"));
    }

    #[test]
    fn test_build_summary_comment_false_positive() {
        let results = vec![FullEvalBuildResult {
            attr: "prebroken".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), false, "error".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: true,
            is_non_deterministic: false,
            exists_on_base: true,
        }];
        let summary = build_summary_comment(123, &results, None, Some("abc123def456"), None, None, None, None);
        assert!(summary.contains("0/1 packages passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("1 pre-existing"));
        assert!(summary.contains("Pre-existing Failures"));
        assert!(summary.contains("prebroken"));
        assert!(summary.contains("base commit: `abc123de`")); // Short commit on separate line
        // When there are only false positives, we shouldn't show the "introduced by this PR" section
        assert!(!summary.contains("introduced by this PR"));
    }

    #[test]
    fn test_build_summary_comment_mixed() {
        let results = vec![
            FullEvalBuildResult {
                attr: "passed".to_string(),
                system: "x86_64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
                package_success: true,
                package_logs: "".to_string(),
                is_false_positive: false,
                is_non_deterministic: false,
                exists_on_base: true,
            },
            FullEvalBuildResult {
                attr: "real-fail".to_string(),
                system: "x86_64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), false, "error".to_string())],
                package_success: false,
                package_logs: "".to_string(),
                is_false_positive: false,
                is_non_deterministic: false,
                exists_on_base: true,
            },
            FullEvalBuildResult {
                attr: "false-positive".to_string(),
                system: "x86_64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), false, "error".to_string())],
                package_success: false,
                package_logs: "".to_string(),
                is_false_positive: true,
                is_non_deterministic: false,
                exists_on_base: true,
            },
        ];
        let summary = build_summary_comment(123, &results, None, None, None, None, None, None);
        assert!(summary.contains("1/3 packages passed"));
        assert!(summary.contains("2 failed"));
        assert!(summary.contains("1 pre-existing"));
        assert!(summary.contains("introduced by this PR"));
        assert!(summary.contains("real-fail"));
        assert!(summary.contains("Pre-existing Failures"));
        assert!(summary.contains("false-positive"));
    }

    #[test]
    fn test_build_summary_comment_with_log_urls() {
        let results = vec![FullEvalBuildResult {
            attr: "python3Packages.broken".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), false, "error".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: false,
            is_non_deterministic: false,
            exists_on_base: true,
        }];
        // With log_url_base and base_commit_short, we get labeled before/after links
        let summary = build_summary_comment(
            123,
            &results,
            Some("https://example.com/logs/123"),
            None,
            None,
            Some("abc12345"),
            None,
            None,
        );
        // Check for the labeled format with before/after links for failed steps
        assert!(summary.contains("src (failed): [before](https://example.com/logs/123/x86_64-linux.python3Packages_broken.src.base-abc12345.log), [after](https://example.com/logs/123/x86_64-linux.python3Packages_broken.src.log)"));
        assert!(summary.contains("package (failed): [before](https://example.com/logs/123/x86_64-linux.python3Packages_broken.package.base-abc12345.log), [after](https://example.com/logs/123/x86_64-linux.python3Packages_broken.package.log)"));
    }

    #[test]
    fn test_build_summary_comment_passed_with_log_urls() {
        let results = vec![FullEvalBuildResult {
            attr: "hello".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
            package_success: true,
            package_logs: "".to_string(),
            is_false_positive: false,
            is_non_deterministic: false,
            exists_on_base: true,
        }];
        // Passed packages get simple step links (no before/after) with system prefix
        let summary = build_summary_comment(
            123,
            &results,
            Some("https://example.com/logs/123"),
            None,
            None,
            Some("abc12345"),
            None,
            None,
        );
        assert!(summary.contains("[src](https://example.com/logs/123/x86_64-linux.hello.src.log)"));
        assert!(summary.contains("[package](https://example.com/logs/123/x86_64-linux.hello.package.log)"));
    }

    #[test]
    fn test_build_summary_comment_with_cli_command() {
        let results = vec![FullEvalBuildResult {
            attr: "hello".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
            package_success: true,
            package_logs: "".to_string(),
            is_false_positive: false,
            is_non_deterministic: false,
            exists_on_base: true,
        }];
        let cli_cmd = "srcbot verify --full-eval --prs 12345 --remote-builder root@host --remote-system aarch64-linux";
        let summary = build_summary_comment(123, &results, None, None, None, None, Some(cli_cmd), None);
        // CLI command should be in a collapsible section
        assert!(summary.contains("<details>"));
        assert!(summary.contains("<summary>Command</summary>"));
        assert!(summary.contains("```bash"));
        // Command is now formatted with line breaks
        assert!(summary.contains("srcbot verify"));
        assert!(summary.contains("--full-eval"));
        assert!(summary.contains("--prs 12345"));
        assert!(summary.contains("--remote-builder root@host"));
        assert!(summary.contains("--remote-system aarch64-linux"));
        assert!(summary.contains("\\\n")); // Line continuation
        assert!(summary.contains("</details>"));
    }

    #[test]
    fn test_build_summary_comment_multi_arch() {
        let remote_config = RemoteBuilderConfig {
            ssh_target: "user@arm-server".to_string(),
            system: "aarch64-linux".to_string(),
            max_jobs: 4,
            gc_threshold: None,
            gc_keep_days: None,
        };
        let results = vec![
            FullEvalBuildResult {
                attr: "hello".to_string(),
                system: "x86_64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
                package_success: true,
                package_logs: "".to_string(),
                is_false_positive: false,
                is_non_deterministic: false,
                exists_on_base: true,
            },
            FullEvalBuildResult {
                attr: "hello".to_string(),
                system: "aarch64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
                package_success: true,
                package_logs: "".to_string(),
                is_false_positive: false,
                is_non_deterministic: false,
                exists_on_base: true,
            },
        ];
        let summary = build_summary_comment(123, &results, None, None, None, None, None, Some(&remote_config));
        // Should have system headers
        assert!(summary.contains("### x86_64-linux"));
        assert!(summary.contains("### aarch64-linux"));
        // Should mention multiple architectures
        assert!(summary.contains("across 2 architectures"));
    }
}
