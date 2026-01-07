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
    let failed: Vec<_> = results.iter().filter(|r| !r.package_success).collect();

    let mut summary = format!(
        "## srcbot: Full Evaluation Results for PR #{}\n\n**Status**: {}/{} packages passed, {} failed\n\n",
        pr_num,
        passed.len(),
        results.len(),
        failed.len()
    );

    if !failed.is_empty() {
        summary.push_str("### Failed Packages\n\n| Package | Failed Step |\n|---------|-------------|\n");
        for result in &failed {
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
