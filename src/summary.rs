use crate::types::FullEvalBuildResult;

/// Build a log URL for an attribute and step
fn build_log_url(log_url_base: &str, attr: &str, step: &str) -> String {
    let attr_safe = attr.replace('.', "_").replace('/', "_");
    format!("{}/{}.{}.log", log_url_base, attr_safe, step)
}

/// Build the summary comment to post to the PR
pub fn build_summary_comment(
    pr_num: u64,
    results: &[FullEvalBuildResult],
    log_url_base: Option<&str>,
) -> String {
    let passed: Vec<_> = results.iter().filter(|r| r.package_success).collect();
    // Real failures: failed AND not a false positive
    let real_failed: Vec<_> = results
        .iter()
        .filter(|r| !r.package_success && !r.is_false_positive)
        .collect();
    // False positives: failed AND is a false positive (pre-existing failure)
    let false_positives: Vec<_> = results
        .iter()
        .filter(|r| !r.package_success && r.is_false_positive)
        .collect();

    let total_failed = real_failed.len() + false_positives.len();

    let mut summary = format!(
        "## srcbot: Full Evaluation Results for PR #{}\n\n**Status**: {}/{} packages passed, {} failed",
        pr_num,
        passed.len(),
        results.len(),
        total_failed
    );

    if !false_positives.is_empty() {
        summary.push_str(&format!(" ({} pre-existing)", false_positives.len()));
    }
    summary.push_str("\n\n");

    // Show real failures first (introduced by this PR)
    if !real_failed.is_empty() {
        summary.push_str("<details>\n<summary>");
        summary.push_str(&format!(
            "Failed Packages (introduced by this PR) - {} packages</summary>\n\n",
            real_failed.len()
        ));

        if log_url_base.is_some() {
            summary.push_str("| Package | Failed Step | Log |\n|---------|-------------|-----|\n");
        } else {
            summary.push_str("| Package | Failed Step |\n|---------|-------------|\n");
        }

        for result in &real_failed {
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");

            if let Some(base) = log_url_base {
                let log_url = build_log_url(base, &result.attr, failed_step);
                summary.push_str(&format!(
                    "| {} | {} | [log]({}) |\n",
                    result.attr, failed_step, log_url
                ));
            } else {
                summary.push_str(&format!("| {} | {} |\n", result.attr, failed_step));
            }
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
        summary.push_str("These packages also fail on the base branch.\n\n");

        if log_url_base.is_some() {
            summary.push_str("| Package | Failed Step | Log |\n|---------|-------------|-----|\n");
        } else {
            summary.push_str("| Package | Failed Step |\n|---------|-------------|\n");
        }

        for result in &false_positives {
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");

            if let Some(base) = log_url_base {
                let log_url = build_log_url(base, &result.attr, failed_step);
                summary.push_str(&format!(
                    "| {} | {} | [log]({}) |\n",
                    result.attr, failed_step, log_url
                ));
            } else {
                summary.push_str(&format!("| {} | {} |\n", result.attr, failed_step));
            }
        }
        summary.push_str("\n</details>\n\n");
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

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_summary_comment_all_passed() {
        let results = vec![FullEvalBuildResult {
            attr: "hello".to_string(),
            intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
            package_success: true,
            package_logs: "".to_string(),
            is_false_positive: false,
        }];
        let summary = build_summary_comment(123, &results, None);
        assert!(summary.contains("1/1 packages passed"));
        assert!(summary.contains("0 failed"));
        assert!(!summary.contains("pre-existing"));
    }

    #[test]
    fn test_build_summary_comment_real_failure() {
        let results = vec![FullEvalBuildResult {
            attr: "broken".to_string(),
            intermediate_results: vec![("src".to_string(), false, "error".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: false,
        }];
        let summary = build_summary_comment(123, &results, None);
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
            intermediate_results: vec![("src".to_string(), false, "error".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: true,
        }];
        let summary = build_summary_comment(123, &results, None);
        assert!(summary.contains("0/1 packages passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("1 pre-existing"));
        assert!(summary.contains("Pre-existing Failures"));
        assert!(summary.contains("prebroken"));
        // When there are only false positives, we shouldn't show the "introduced by this PR" section
        assert!(!summary.contains("introduced by this PR"));
    }

    #[test]
    fn test_build_summary_comment_mixed() {
        let results = vec![
            FullEvalBuildResult {
                attr: "passed".to_string(),
                intermediate_results: vec![("src".to_string(), true, "ok".to_string())],
                package_success: true,
                package_logs: "".to_string(),
                is_false_positive: false,
            },
            FullEvalBuildResult {
                attr: "real-fail".to_string(),
                intermediate_results: vec![("src".to_string(), false, "error".to_string())],
                package_success: false,
                package_logs: "".to_string(),
                is_false_positive: false,
            },
            FullEvalBuildResult {
                attr: "false-positive".to_string(),
                intermediate_results: vec![("src".to_string(), false, "error".to_string())],
                package_success: false,
                package_logs: "".to_string(),
                is_false_positive: true,
            },
        ];
        let summary = build_summary_comment(123, &results, None);
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
            intermediate_results: vec![("src".to_string(), false, "error".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: false,
        }];
        let summary = build_summary_comment(123, &results, Some("https://example.com/logs/123"));
        assert!(summary.contains("[log](https://example.com/logs/123/python3Packages_broken.src.log)"));
    }
}
