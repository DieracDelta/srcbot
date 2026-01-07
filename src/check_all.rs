//! check-all subcommand implementation
//!
//! Builds all intermediate attributes for packages in a specified pkgset.

use anyhow::Result;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{info, warn};

use crate::cli::CheckAllArgs;
use crate::full_eval_types::EvalJobOutput;
use crate::nix::{build_intermediate_async, run_nix_eval_jobs_pkgset};
use crate::types::INTERMEDIATE_ATTRS;

/// Type of failure encountered during a build
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FailureType {
    HashMismatch,
    NotFound404,
    EvalError,
    Unknown,
}

impl std::fmt::Display for FailureType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FailureType::HashMismatch => write!(f, "Hash mismatch"),
            FailureType::NotFound404 => write!(f, "404 Not Found"),
            FailureType::EvalError => write!(f, "Eval error"),
            FailureType::Unknown => write!(f, "Unknown"),
        }
    }
}

/// A single build failure record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildFailure {
    pub attr: String,
    pub intermediate: String,
    pub failure_type: FailureType,
    pub log_path: Option<PathBuf>,
    pub error_snippet: String,
}

/// Summary statistics for an intermediate attribute
#[derive(Debug, Clone, Default)]
pub struct IntermediateSummary {
    pub attempted: usize,
    pub passed: usize,
    pub failed: usize,
    pub failures: Vec<BuildFailure>,
}

/// Summary statistics for check-all results
#[derive(Debug, Clone, Default)]
pub struct CheckAllSummary {
    pub total_packages: usize,
    pub eval_errors: usize,
    pub per_intermediate: HashMap<String, IntermediateSummary>,
}

/// Classify a build failure based on log content
fn classify_failure(logs: &str) -> FailureType {
    let logs_lower = logs.to_lowercase();
    if logs_lower.contains("hash mismatch")
        || logs_lower.contains("got:")
        || logs_lower.contains("specified:")
    {
        FailureType::HashMismatch
    } else if logs_lower.contains("404")
        || logs_lower.contains("not found")
        || logs_lower.contains("could not be found")
        || logs_lower.contains("does not exist")
    {
        FailureType::NotFound404
    } else {
        FailureType::Unknown
    }
}

/// Print the summary table to stdout
fn print_summary_table(summary: &CheckAllSummary, pkgset: &str) {
    println!();
    println!("============================================================");
    println!(
        "           check-all Summary: {}",
        if pkgset.is_empty() {
            "all packages"
        } else {
            pkgset
        }
    );
    println!("============================================================");
    println!("Total packages evaluated: {}", summary.total_packages);
    println!("Evaluation errors: {}", summary.eval_errors);
    println!();

    println!(
        "{:<16} | {:>9} | {:>6} | {:>6} | {:>9}",
        "Intermediate", "Attempted", "Passed", "Failed", "Pass Rate"
    );
    println!("{}", "-".repeat(60));

    for intermediate in INTERMEDIATE_ATTRS {
        if let Some(s) = summary.per_intermediate.get(*intermediate) {
            if s.attempted > 0 {
                let rate = (s.passed as f64 / s.attempted as f64) * 100.0;
                println!(
                    "{:<16} | {:>9} | {:>6} | {:>6} | {:>8.1}%",
                    intermediate, s.attempted, s.passed, s.failed, rate
                );
            }
        }
    }

    // Failure breakdown by type
    let mut by_type: HashMap<String, usize> = HashMap::new();
    for s in summary.per_intermediate.values() {
        for f in &s.failures {
            *by_type.entry(f.failure_type.to_string()).or_default() += 1;
        }
    }

    if !by_type.is_empty() {
        println!();
        println!("Failure Breakdown:");
        for (typ, count) in by_type {
            println!("  {}: {}", typ, count);
        }
    }
    println!();
}

/// Write failures to a JSON file
fn write_failures_file(summary: &CheckAllSummary, path: &PathBuf) -> Result<()> {
    let failures: Vec<&BuildFailure> = summary
        .per_intermediate
        .values()
        .flat_map(|s| s.failures.iter())
        .collect();

    let json = serde_json::to_string_pretty(&failures)?;
    std::fs::write(path, json)?;
    info!("Wrote {} failures to {:?}", failures.len(), path);
    Ok(())
}

/// Main entry point for check-all command
pub async fn process_check_all(args: &CheckAllArgs) -> Result<bool> {
    info!(
        "srcbot check-all: Checking {} packages in {}",
        if args.pkgset.is_empty() {
            "all"
        } else {
            &args.pkgset
        },
        args.nixpkgs.display()
    );

    // Canonicalize the nixpkgs path
    let nixpkgs_path = args.nixpkgs.canonicalize()?;

    // Step 1: Evaluate all packages in pkgset
    let packages = run_nix_eval_jobs_pkgset(
        &nixpkgs_path,
        &args.pkgset,
        &args.system,
        args.eval_workers,
    )
    .await?;

    // Step 2: Collect packages with their intermediates
    let mut summary = CheckAllSummary {
        total_packages: packages.len(),
        ..Default::default()
    };

    // Filter out eval errors
    let valid_packages: Vec<&EvalJobOutput> =
        packages.iter().filter(|p| p.error.is_none()).collect();
    summary.eval_errors = packages.len() - valid_packages.len();

    // Apply limit if specified
    let valid_packages: Vec<&EvalJobOutput> = match args.limit {
        Some(limit) => valid_packages.into_iter().take(limit).collect(),
        None => valid_packages,
    };

    info!(
        "Evaluated {} packages ({} valid{}, {} errors)",
        packages.len(),
        valid_packages.len(),
        args.limit
            .map(|l| format!(", limited to {}", l))
            .unwrap_or_default(),
        summary.eval_errors
    );

    // Step 3: Build intermediates tier by tier
    for intermediate in INTERMEDIATE_ATTRS {
        // Find packages that have this intermediate
        let packages_with_intermediate: Vec<&EvalJobOutput> = valid_packages
            .iter()
            .filter(|p| {
                p.extra_value
                    .as_ref()
                    .and_then(|ev| ev.get(*intermediate))
                    .and_then(|v| v.as_ref())
                    .is_some()
            })
            .copied()
            .collect();

        if packages_with_intermediate.is_empty() {
            continue;
        }

        let mut intermediate_summary = IntermediateSummary {
            attempted: packages_with_intermediate.len(),
            ..Default::default()
        };

        info!(
            "Building {} packages' .{} attribute...",
            packages_with_intermediate.len(),
            intermediate
        );

        // Build with parallelism using buffer_unordered
        // Attribute paths from nix-eval-jobs are relative to the pkgset,
        // so we need to prepend the pkgset to get the full path
        let pkgset = args.pkgset.clone();
        let nixpkgs = nixpkgs_path.clone();
        let system = args.system.clone();
        let intermediate_str = intermediate.to_string();

        let mut stream = stream::iter(packages_with_intermediate.iter().map(|pkg| {
            let full_attr = if pkgset.is_empty() {
                pkg.attr.clone()
            } else {
                format!("{}.{}", pkgset, pkg.attr)
            };
            build_intermediate_async(
                nixpkgs.clone(),
                full_attr,
                intermediate_str.clone(),
                system.clone(),
                0, // No PR number for check-all
                true,
            )
        }))
        .buffer_unordered(args.build_jobs);

        while let Some((attr, intermediate_name, success, logs)) = stream.next().await {
            if success {
                intermediate_summary.passed += 1;
            } else {
                intermediate_summary.failed += 1;
                let failure = BuildFailure {
                    attr: attr.clone(),
                    intermediate: intermediate_name.clone(),
                    failure_type: classify_failure(&logs),
                    log_path: None,
                    error_snippet: logs.lines().take(10).collect::<Vec<_>>().join("\n"),
                };

                // Log failure immediately to stdout
                warn!(
                    "FAILED {}.{}: {}",
                    attr, intermediate_name, failure.failure_type
                );

                intermediate_summary.failures.push(failure);
            }
        }

        summary
            .per_intermediate
            .insert(intermediate.to_string(), intermediate_summary);
    }

    // Step 4: Print summary table
    print_summary_table(&summary, &args.pkgset);

    // Step 5: Write failures file if requested
    if let Some(ref failures_path) = args.failures_file {
        write_failures_file(&summary, failures_path)?;
    }

    // Return success only if no failures
    let total_failures: usize = summary.per_intermediate.values().map(|s| s.failed).sum();
    Ok(total_failures == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_failure_hash_mismatch() {
        // Test various hash mismatch patterns
        assert!(matches!(
            classify_failure("error: hash mismatch in fixed-output derivation"),
            FailureType::HashMismatch
        ));
        assert!(matches!(
            classify_failure("got: sha256-abc123..."),
            FailureType::HashMismatch
        ));
        assert!(matches!(
            classify_failure("specified: sha256-xyz789..."),
            FailureType::HashMismatch
        ));
        assert!(matches!(
            classify_failure("Hash Mismatch detected"),
            FailureType::HashMismatch
        ));
    }

    #[test]
    fn test_classify_failure_404() {
        // Test various 404/not found patterns
        assert!(matches!(
            classify_failure("HTTP error 404"),
            FailureType::NotFound404
        ));
        assert!(matches!(
            classify_failure("file not found"),
            FailureType::NotFound404
        ));
        assert!(matches!(
            classify_failure("resource could not be found"),
            FailureType::NotFound404
        ));
        assert!(matches!(
            classify_failure("path does not exist"),
            FailureType::NotFound404
        ));
    }

    #[test]
    fn test_classify_failure_unknown() {
        // Test that unrecognized patterns return Unknown
        assert!(matches!(
            classify_failure("some random error"),
            FailureType::Unknown
        ));
        assert!(matches!(
            classify_failure("compilation failed"),
            FailureType::Unknown
        ));
        assert!(matches!(classify_failure(""), FailureType::Unknown));
    }

    #[test]
    fn test_failure_type_display() {
        assert_eq!(FailureType::HashMismatch.to_string(), "Hash mismatch");
        assert_eq!(FailureType::NotFound404.to_string(), "404 Not Found");
        assert_eq!(FailureType::EvalError.to_string(), "Eval error");
        assert_eq!(FailureType::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn test_build_failure_serialize_deserialize() {
        let failure = BuildFailure {
            attr: "python3Packages.requests".to_string(),
            intermediate: "src".to_string(),
            failure_type: FailureType::HashMismatch,
            log_path: Some(PathBuf::from("/tmp/logs/requests.log")),
            error_snippet: "hash mismatch\ngot: sha256-abc".to_string(),
        };

        let json = serde_json::to_string(&failure).unwrap();
        let deserialized: BuildFailure = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.attr, "python3Packages.requests");
        assert_eq!(deserialized.intermediate, "src");
        assert!(matches!(deserialized.failure_type, FailureType::HashMismatch));
        assert_eq!(
            deserialized.log_path,
            Some(PathBuf::from("/tmp/logs/requests.log"))
        );
        assert_eq!(deserialized.error_snippet, "hash mismatch\ngot: sha256-abc");
    }

    #[test]
    fn test_build_failure_serialize_no_log_path() {
        let failure = BuildFailure {
            attr: "hello".to_string(),
            intermediate: "src".to_string(),
            failure_type: FailureType::NotFound404,
            log_path: None,
            error_snippet: "404 error".to_string(),
        };

        let json = serde_json::to_string(&failure).unwrap();
        let deserialized: BuildFailure = serde_json::from_str(&json).unwrap();

        assert!(deserialized.log_path.is_none());
    }

    #[test]
    fn test_intermediate_summary_default() {
        let summary = IntermediateSummary::default();
        assert_eq!(summary.attempted, 0);
        assert_eq!(summary.passed, 0);
        assert_eq!(summary.failed, 0);
        assert!(summary.failures.is_empty());
    }

    #[test]
    fn test_check_all_summary_default() {
        let summary = CheckAllSummary::default();
        assert_eq!(summary.total_packages, 0);
        assert_eq!(summary.eval_errors, 0);
        assert!(summary.per_intermediate.is_empty());
    }

    #[test]
    fn test_check_all_summary_with_data() {
        let mut summary = CheckAllSummary {
            total_packages: 100,
            eval_errors: 5,
            per_intermediate: HashMap::new(),
        };

        let src_summary = IntermediateSummary {
            attempted: 95,
            passed: 90,
            failed: 5,
            failures: vec![BuildFailure {
                attr: "broken-pkg".to_string(),
                intermediate: "src".to_string(),
                failure_type: FailureType::HashMismatch,
                log_path: None,
                error_snippet: "error".to_string(),
            }],
        };

        summary
            .per_intermediate
            .insert("src".to_string(), src_summary.clone());

        assert_eq!(summary.total_packages, 100);
        assert_eq!(summary.eval_errors, 5);
        assert_eq!(summary.per_intermediate.len(), 1);

        let src = summary.per_intermediate.get("src").unwrap();
        assert_eq!(src.attempted, 95);
        assert_eq!(src.passed, 90);
        assert_eq!(src.failed, 5);
        assert_eq!(src.failures.len(), 1);
    }

    #[test]
    fn test_failure_type_serialize_deserialize() {
        // Test that all FailureType variants can be serialized and deserialized
        let types = vec![
            FailureType::HashMismatch,
            FailureType::NotFound404,
            FailureType::EvalError,
            FailureType::Unknown,
        ];

        for ft in types {
            let json = serde_json::to_string(&ft).unwrap();
            let deserialized: FailureType = serde_json::from_str(&json).unwrap();
            // Check they serialize to expected strings
            match (&ft, &deserialized) {
                (FailureType::HashMismatch, FailureType::HashMismatch) => {}
                (FailureType::NotFound404, FailureType::NotFound404) => {}
                (FailureType::EvalError, FailureType::EvalError) => {}
                (FailureType::Unknown, FailureType::Unknown) => {}
                _ => panic!("Mismatch after serialization round-trip"),
            }
        }
    }

    #[test]
    fn test_classify_failure_case_insensitive() {
        // Verify case insensitivity
        assert!(matches!(
            classify_failure("HASH MISMATCH"),
            FailureType::HashMismatch
        ));
        assert!(matches!(
            classify_failure("NOT FOUND"),
            FailureType::NotFound404
        ));
        assert!(matches!(
            classify_failure("Got: sha256-..."),
            FailureType::HashMismatch
        ));
    }
}
