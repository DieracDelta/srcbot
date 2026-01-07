use std::collections::HashMap;

use crate::types::FullEvalBuildResult;

/// Build a summary of intermediate build results (for posting before final builds)
pub fn build_intermediate_summary(pr_num: u64, intermediate_results: &HashMap<String, Vec<(String, bool, String)>>) -> String {
    let mut all_passed = 0;
    let mut all_failed = 0;
    let mut failed_details: Vec<(String, String)> = Vec::new(); // (attr, failed_intermediate)
    let mut passed_details: Vec<(String, Vec<String>)> = Vec::new(); // (attr, intermediates)

    for (attr, results) in intermediate_results {
        let failed: Vec<_> = results.iter().filter(|(_, success, _)| !success).collect();
        let passed: Vec<_> = results.iter().filter(|(_, success, _)| *success).collect();

        if failed.is_empty() {
            all_passed += 1;
            passed_details.push((attr.clone(), passed.iter().map(|(name, _, _)| name.clone()).collect()));
        } else {
            all_failed += 1;
            if let Some((name, _, _)) = failed.first() {
                failed_details.push((attr.clone(), name.clone()));
            }
        }
    }

    let mut summary = format!(
        "## srcbot: Intermediate Build Results for PR #{}\n\n\
        **Status**: {}/{} packages passed intermediate builds, {} failed\n\n\
        ⏳ **Final package builds will follow in a separate comment.**\n\n",
        pr_num,
        all_passed,
        intermediate_results.len(),
        all_failed
    );

    if !failed_details.is_empty() {
        summary.push_str("### Failed Intermediate Builds\n\n| Package | Failed Step |\n|---------|-------------|\n");
        for (attr, step) in &failed_details {
            summary.push_str(&format!("| {} | {} |\n", attr, step));
        }
        summary.push('\n');
    }

    if !passed_details.is_empty() {
        summary.push_str("<details>\n<summary>");
        summary.push_str(&format!("{} packages passed intermediate builds</summary>\n\n", all_passed));
        summary.push_str("| Package | Intermediates Built |\n|---------|--------------------|\n");
        for (attr, intermediates) in &passed_details {
            summary.push_str(&format!("| {} | {} |\n", attr, intermediates.join(", ")));
        }
        summary.push_str("\n</details>\n");
    }

    summary
}

/// Build the summary comment to post to the PR
pub fn build_summary_comment(pr_num: u64, results: &[FullEvalBuildResult]) -> String {
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
        summary.push_str(&format!(
            " ({} pre-existing)",
            false_positives.len()
        ));
    }
    summary.push_str("\n\n");

    // Show real failures first (introduced by this PR)
    if !real_failed.is_empty() {
        summary.push_str("### ❌ Failed Packages (introduced by this PR)\n\n| Package | Failed Step |\n|---------|-------------|\n");
        for result in &real_failed {
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");
            summary.push_str(&format!("| {} | {} |\n", result.attr, failed_step));
        }
        summary.push('\n');
    }

    // Show false positives (pre-existing failures)
    if !false_positives.is_empty() {
        summary.push_str("### ⚠️ Pre-existing Failures (false positives)\n\nThese packages also fail on the base branch (pre-existing issues).\n\n| Package | Failed Step |\n|---------|-------------|\n");
        for result in &false_positives {
            let failed_step = result
                .intermediate_results
                .iter()
                .find(|(_, success, _)| !success)
                .map(|(name, _, _)| name.as_str())
                .unwrap_or("package");
            summary.push_str(&format!("| {} | {} |\n", result.attr, failed_step));
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
        let summary = build_summary_comment(123, &results);
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
        let summary = build_summary_comment(123, &results);
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
        let summary = build_summary_comment(123, &results);
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
        let summary = build_summary_comment(123, &results);
        assert!(summary.contains("1/3 packages passed"));
        assert!(summary.contains("2 failed"));
        assert!(summary.contains("1 pre-existing"));
        assert!(summary.contains("introduced by this PR"));
        assert!(summary.contains("real-fail"));
        assert!(summary.contains("Pre-existing Failures"));
        assert!(summary.contains("false-positive"));
    }

    #[test]
    fn test_build_intermediate_summary() {
        let mut intermediate_results = HashMap::new();
        intermediate_results.insert(
            "hello".to_string(),
            vec![("src".to_string(), true, "ok".to_string())],
        );
        intermediate_results.insert(
            "broken".to_string(),
            vec![("src".to_string(), false, "error".to_string())],
        );

        let summary = build_intermediate_summary(123, &intermediate_results);
        assert!(summary.contains("1/2 packages passed"));
        assert!(summary.contains("1 failed"));
        assert!(summary.contains("hello"));
        assert!(summary.contains("broken"));
    }
}
