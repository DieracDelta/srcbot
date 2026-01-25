use crate::types::{FullEvalBuildResult, RemoteBuilderConfig};
use std::collections::HashMap;

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
/// For failures (include_before_after=true): `src ([before](...), [after](...)), package ([before](...), [after](...))`
/// For passed (include_before_after=false): `[src](...), [package](...)`
fn build_steps_with_links(
    result: &FullEvalBuildResult,
    log_url_base: Option<&str>,
    base_commit_short: Option<&str>,
    include_before_after: bool,
) -> String {
    // Collect all steps: intermediates + package
    let mut steps: Vec<&str> = result
        .intermediate_results
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    steps.push("package");

    // Use result's system for log URLs, default to x86_64-linux if not set
    let system = if result.system.is_empty() {
        "x86_64-linux"
    } else {
        &result.system
    };

    match (log_url_base, base_commit_short, include_before_after) {
        (Some(base), Some(commit_short), true) => {
            // Before/after format for failures
            steps
                .iter()
                .map(|step| {
                    let before_url = build_base_log_url(base, system, &result.attr, step, commit_short);
                    let after_url = build_log_url(base, system, &result.attr, step);
                    format!("{} ([before]({}), [after]({}))", step, before_url, after_url)
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        (Some(base), _, false) => {
            // Simple links for passed packages
            steps
                .iter()
                .map(|step| {
                    let url = build_log_url(base, system, &result.attr, step);
                    format!("[{}]({})", step, url)
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => {
            // No links - just step names
            steps.join(", ")
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
    remote_config: Option<&RemoteBuilderConfig>,
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

    let mut summary = format!("## srcbot: Full Evaluation Results for PR #{}", pr_num);

    // Show base and head commits in header if available
    match (base_commit, head_commit) {
        (Some(base), Some(head)) => {
            let base_short = &base[..8.min(base.len())];
            let head_short = &head[..8.min(head.len())];
            summary.push_str(&format!(" (base: `{}`, head: `{}`)", base_short, head_short));
        }
        (Some(base), None) => {
            let base_short = &base[..8.min(base.len())];
            summary.push_str(&format!(" (base: `{}`)", base_short));
        }
        (None, Some(head)) => {
            let head_short = &head[..8.min(head.len())];
            summary.push_str(&format!(" (head: `{}`)", head_short));
        }
        (None, None) => {}
    }

    summary.push('\n');

    // Add CLI command in collapsible section if provided
    if let Some(cmd) = cli_command {
        if !cmd.is_empty() {
            summary.push_str("\n<details>\n<summary>Command</summary>\n\n```bash\n");
            summary.push_str(cmd);
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
            let system_suffix = if remote_config.map(|rc| &rc.system) == Some(system) {
                format!(" (remote: {})", remote_config.unwrap().ssh_target)
            } else {
                " (local)".to_string()
            };
            summary.push_str(&format!("### {}{}\n\n", system, system_suffix));
        }

        let passed: Vec<_> = system_results
            .iter()
            .filter(|r| r.package_success)
            .collect();
        let real_failed: Vec<_> = system_results
            .iter()
            .filter(|r| !r.package_success && !r.is_false_positive)
            .collect();
        let false_positives: Vec<_> = system_results
            .iter()
            .filter(|r| !r.package_success && r.is_false_positive)
            .collect();

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
        }];
        let summary = build_summary_comment(123, &results, None, Some("abc123def456"), None, None, None, None);
        assert!(summary.contains("0/1 packages passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("1 pre-existing"));
        assert!(summary.contains("Pre-existing Failures"));
        assert!(summary.contains("prebroken"));
        assert!(summary.contains("(base: `abc123de`)")); // Short commit in header
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
            },
            FullEvalBuildResult {
                attr: "real-fail".to_string(),
                system: "x86_64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), false, "error".to_string())],
                package_success: false,
                package_logs: "".to_string(),
                is_false_positive: false,
                is_non_deterministic: false,
            },
            FullEvalBuildResult {
                attr: "false-positive".to_string(),
                system: "x86_64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), false, "error".to_string())],
                package_success: false,
                package_logs: "".to_string(),
                is_false_positive: true,
                is_non_deterministic: false,
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
        }];
        // With log_url_base and base_commit_short, we get before/after links with system prefix
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
        // Check for the new before/after format with system prefix
        assert!(summary.contains("src ([before](https://example.com/logs/123/x86_64-linux.python3Packages_broken.src.base-abc12345.log), [after](https://example.com/logs/123/x86_64-linux.python3Packages_broken.src.log))"));
        assert!(summary.contains("package ([before](https://example.com/logs/123/x86_64-linux.python3Packages_broken.package.base-abc12345.log), [after](https://example.com/logs/123/x86_64-linux.python3Packages_broken.package.log))"));
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
        }];
        let cli_cmd = "srcbot verify --full-eval --prs 12345 --remote-builder root@host --remote-system aarch64-linux";
        let summary = build_summary_comment(123, &results, None, None, None, None, Some(cli_cmd), None);
        // CLI command should be in a collapsible section
        assert!(summary.contains("<details>"));
        assert!(summary.contains("<summary>Command</summary>"));
        assert!(summary.contains("```bash"));
        assert!(summary.contains(cli_cmd));
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
            },
            FullEvalBuildResult {
                attr: "hello".to_string(),
                system: "aarch64-linux".to_string(),
                intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
                package_success: true,
                package_logs: "".to_string(),
                is_false_positive: false,
                is_non_deterministic: false,
            },
        ];
        let summary = build_summary_comment(123, &results, None, None, None, None, None, Some(&remote_config));
        // Should have system headers
        assert!(summary.contains("### x86_64-linux (local)"));
        assert!(summary.contains("### aarch64-linux (remote: user@arm-server)"));
        // Should mention multiple architectures
        assert!(summary.contains("across 2 architectures"));
    }
}
