use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "srcbot")]
#[command(about = "Verify nixpkgs PR source fetches correctly by rebuilding with fresh sources")]
pub struct Args {
    /// Custom location for eval cache, logs, and state (defaults to ~/.cache/srcbot)
    #[arg(long, global = true)]
    pub save_location: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Verify PR source fetches by rebuilding packages
    Verify(VerifyArgs),

    /// Fix a hash mismatch for a known attribute
    FixHash(FixHashArgs),

    /// Check all intermediate attributes in a package set
    CheckAll(CheckAllArgs),
}

#[derive(Parser, Debug)]
pub struct VerifyArgs {
    /// GitHub PR numbers (comma separated)
    #[arg(long, value_delimiter = ',')]
    pub prs: Vec<u64>,

    /// Nixpkgs attribute to verify (e.g., "hello", "python3Packages.requests")
    #[arg(short, long)]
    pub attr: Option<String>,

    /// GitHub token for posting comments (uses GITHUB_TOKEN env var if not provided)
    #[arg(long, env = "GITHUB_TOKEN")]
    pub token: Option<String>,

    /// Path to local nixpkgs checkout (will fetch PR into this repo)
    #[arg(long, default_value = ".")]
    pub nixpkgs: PathBuf,

    /// Don't post comment to GitHub, just print result
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// System to build for
    #[arg(long, default_value = "x86_64-linux")]
    pub system: String,

    /// Enable full evaluation mode to detect all changed packages (for treewide PRs)
    #[arg(long, default_value_t = false)]
    pub full_eval: bool,

    /// Number of parallel workers for nix-eval-jobs (only used with --full-eval)
    #[arg(long, default_value_t = num_cpus::get())]
    pub eval_workers: usize,

    /// Post results to a GitHub gist (only used with --full-eval)
    #[arg(long, default_value_t = false)]
    pub gist: bool,

    /// Maximum parallel final package builds (only used with --full-eval)
    #[arg(long, default_value_t = 4)]
    pub build_jobs: usize,

    /// Resume from saved state (only used with --full-eval)
    #[arg(long, default_value_t = false)]
    pub resume: bool,

    /// Rebuild everything from scratch, ignoring cache (slower but more thorough).
    /// By default, srcbot allows dependencies to come from cache and uses --check
    /// to verify FODs that were substituted.
    #[arg(long, default_value_t = false)]
    pub full_rebuild: bool,

    /// When a build fails, check if it also fails on the base branch.
    /// If so, mark it as a "false positive" (pre-existing failure) and show separately.
    /// Only used with --full-eval.
    #[arg(long, default_value_t = false)]
    pub false_positive: bool,

    /// Override the base commit for false positive detection.
    /// By default, uses the merge-base between the PR and the target branch.
    /// Only used with --full-eval --false-positive.
    #[arg(long)]
    pub base_commit: Option<String>,

    /// Also detect and build packages where the final drvPath changed (not just intermediates).
    /// Only used with --full-eval.
    #[arg(long, default_value_t = false)]
    pub verify_full_drvs: bool,

    /// When any derivation changes, also rebuild all intermediate attributes (src, goModules, etc.)
    /// even if those specific intermediates didn't change. This verifies upstream sources are still
    /// fetchable and match expected hashes. Enabled by default.
    /// Only used with --full-eval.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub aggressively_check_fods: bool,

    /// Skip building passthru.tests for changed packages.
    /// By default, tests are built and test failures cause verification to fail.
    #[arg(long, default_value_t = false)]
    pub skip_tests: bool,

    /// Timeout in seconds for nix operations (builds, evals, etc.).
    /// Operations that exceed this timeout will be cancelled and marked as failed.
    #[arg(long, default_value_t = 3600)]
    pub nix_timeout: u64,

    /// Base URL for log files (e.g., "https://example.com/srcbot-srv")
    /// When provided, log links will be added to the summary tables.
    /// Only used with --full-eval.
    #[arg(long)]
    pub log_base_url: Option<String>,

    /// Remote builder SSH target (e.g., "root@host", "user@192.168.1.1")
    /// When specified with --remote-system, builds for that architecture will be
    /// sent to the remote machine via Nix's --builders flag.
    #[arg(long = "remote-builder")]
    pub remote_builder: Option<String>,

    /// Remote system architecture (e.g., "aarch64-linux")
    /// Must be specified together with --remote-builder.
    #[arg(long = "remote-system")]
    pub remote_system: Option<String>,

    /// Maximum parallel jobs on the remote builder (default: 4)
    #[arg(long = "remote-build-jobs", default_value_t = 4)]
    pub remote_build_jobs: usize,

    /// Trigger GC on remote when disk exceeds threshold (e.g., "50G", "100GB")
    #[arg(long)]
    pub remote_gc_threshold: Option<String>,

    /// Keep builds newer than N days during remote GC
    #[arg(long)]
    pub remote_gc_keep_days: Option<u64>,
}

#[derive(Parser, Debug)]
pub struct FixHashArgs {
    /// Path to local nixpkgs checkout
    #[arg(long)]
    pub nixpkgs: PathBuf,

    /// Nixpkgs attribute with the hash mismatch (e.g., "hello", "python3Packages.requests")
    #[arg(long)]
    pub attribute: String,

    /// Which intermediate attribute to fix (e.g., "src", "goModules", "cargoDeps")
    /// If not specified, defaults to "src"
    #[arg(long, default_value = "src")]
    pub intermediate: String,

    /// System to build for
    #[arg(long, default_value = "x86_64-linux")]
    pub system: String,

    /// Skip diffing the fetched sources (useful for vendorHash where diff is meaningless)
    #[arg(long, default_value_t = false)]
    pub dont_diff: bool,

    /// Location to write the difftastic diff log
    #[arg(long)]
    pub log_location: Option<PathBuf>,

    /// Git ref/commit to checkout in nixpkgs (defaults to origin/master)
    #[arg(long, default_value = "origin/master")]
    pub nixpkgs_ref: String,

    /// Git remote to push the fix branch to (e.g., "origin", "myfork")
    #[arg(long)]
    pub origin: Option<String>,

    /// Branch name for the fix (defaults to "fix/{attribute}")
    #[arg(long)]
    pub branch: Option<String>,

    /// Base URL for log files in PR description
    #[arg(long, default_value = "https://instance-20251227-2125.tail5ca7.ts.net/srcbot-srv")]
    pub log_base_url: String,

    /// Skip printing PR text to stdout
    #[arg(long, default_value_t = false)]
    pub no_pr_text: bool,

    /// Timeout in seconds for nix operations (builds, evals, etc.).
    /// Operations that exceed this timeout will be cancelled and marked as failed.
    #[arg(long, default_value_t = 3600)]
    pub nix_timeout: u64,

    /// Remote builder SSH target (e.g., "root@host", "user@192.168.1.1")
    /// When specified with --remote-system, builds will be sent to the remote machine.
    #[arg(long = "remote-builder")]
    pub remote_builder: Option<String>,

    /// Remote system architecture (e.g., "aarch64-linux")
    /// Must be specified together with --remote-builder.
    #[arg(long = "remote-system")]
    pub remote_system: Option<String>,

    /// Maximum parallel jobs on the remote builder (default: 4)
    #[arg(long = "remote-build-jobs", default_value_t = 4)]
    pub remote_build_jobs: usize,
}

#[derive(Parser, Debug)]
pub struct CheckAllArgs {
    /// Package set to check (e.g., "python3Packages", "" for root/all packages)
    pub pkgset: String,

    /// Path to nixpkgs checkout
    pub nixpkgs: PathBuf,

    /// System to build for
    #[arg(long, default_value = "x86_64-linux")]
    pub system: String,

    /// Number of parallel workers for nix-eval-jobs
    #[arg(long, default_value_t = num_cpus::get())]
    pub eval_workers: usize,

    /// Maximum parallel builds
    #[arg(long, default_value_t = 4)]
    pub build_jobs: usize,

    /// Optional file path to write failure details as JSON
    #[arg(long)]
    pub failures_file: Option<PathBuf>,

    /// Maximum number of packages to build (useful for testing)
    #[arg(long)]
    pub limit: Option<usize>,

    /// Timeout in seconds for nix operations (builds, evals, etc.).
    /// Operations that exceed this timeout will be cancelled and marked as failed.
    #[arg(long, default_value_t = 3600)]
    pub nix_timeout: u64,

    /// Remote builder SSH target (e.g., "root@host", "user@192.168.1.1")
    /// When specified with --remote-system, builds for that architecture will be
    /// sent to the remote machine via Nix's --builders flag.
    #[arg(long = "remote-builder")]
    pub remote_builder: Option<String>,

    /// Remote system architecture (e.g., "aarch64-linux")
    /// Must be specified together with --remote-builder.
    #[arg(long = "remote-system")]
    pub remote_system: Option<String>,

    /// Maximum parallel jobs on the remote builder (default: 4)
    #[arg(long = "remote-build-jobs", default_value_t = 4)]
    pub remote_build_jobs: usize,

    /// Trigger GC on remote when disk exceeds threshold (e.g., "50G", "100GB")
    #[arg(long)]
    pub remote_gc_threshold: Option<String>,

    /// Keep builds newer than N days during remote GC
    #[arg(long)]
    pub remote_gc_keep_days: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_rebuild_flag_default_false() {
        // Default (no flag) should be false (cache-friendly mode)
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                !verify_args.full_rebuild,
                "--full-rebuild should default to false"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_full_rebuild_flag_true_when_set() {
        // --full-rebuild should be true when explicitly set
        let args =
            Args::try_parse_from(["srcbot", "verify", "--full-rebuild", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                verify_args.full_rebuild,
                "--full-rebuild should be true when set"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_full_rebuild_with_full_eval() {
        // --full-rebuild should work together with --full-eval
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-rebuild",
            "--full-eval",
            "--prs",
            "12345",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(verify_args.full_rebuild, "--full-rebuild should be true");
            assert!(verify_args.full_eval, "--full-eval should be true");
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_verify_args_defaults() {
        // Verify all the important defaults for VerifyArgs
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(!verify_args.full_rebuild, "full_rebuild defaults to false");
            assert!(!verify_args.dry_run, "dry_run defaults to false");
            assert!(!verify_args.full_eval, "full_eval defaults to false");
            assert!(!verify_args.gist, "gist defaults to false");
            assert!(!verify_args.resume, "resume defaults to false");
            assert!(
                !verify_args.false_positive,
                "false_positive defaults to false"
            );
            assert_eq!(verify_args.system, "x86_64-linux");
            assert_eq!(verify_args.build_jobs, 4);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_false_positive_flag_default_false() {
        // Default (no flag) should be false
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                !verify_args.false_positive,
                "--false-positive should default to false"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_false_positive_flag_true_when_set() {
        // --false-positive should be true when explicitly set
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-eval",
            "--false-positive",
            "--prs",
            "12345",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                verify_args.false_positive,
                "--false-positive should be true when set"
            );
            assert!(verify_args.full_eval, "--full-eval should be true");
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_multiple_prs() {
        // Test comma-separated PR numbers
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "123,456,789"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert_eq!(verify_args.prs, vec![123, 456, 789]);
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_check_all_basic() {
        // Basic check-all command with pkgset and nixpkgs path
        let args =
            Args::try_parse_from(["srcbot", "check-all", "python3Packages", "/path/to/nixpkgs"])
                .unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.pkgset, "python3Packages");
            assert_eq!(
                check_args.nixpkgs,
                std::path::PathBuf::from("/path/to/nixpkgs")
            );
        } else {
            panic!("Expected CheckAll command");
        }
    }

    #[test]
    fn test_check_all_empty_pkgset() {
        // Check-all with empty pkgset (root packages)
        let args = Args::try_parse_from(["srcbot", "check-all", "", "."]).unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.pkgset, "");
            assert_eq!(check_args.nixpkgs, std::path::PathBuf::from("."));
        } else {
            panic!("Expected CheckAll command");
        }
    }

    #[test]
    fn test_check_all_defaults() {
        // Verify all the important defaults for CheckAllArgs
        let args = Args::try_parse_from(["srcbot", "check-all", "python3Packages", "."]).unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.system, "x86_64-linux");
            assert_eq!(check_args.build_jobs, 4);
            assert!(check_args.failures_file.is_none());
            assert!(check_args.limit.is_none());
            // eval_workers defaults to num_cpus, just check it's > 0
            assert!(check_args.eval_workers > 0);
        } else {
            panic!("Expected CheckAll command");
        }
    }

    #[test]
    fn test_check_all_with_options() {
        // Check-all with all options specified
        let args = Args::try_parse_from([
            "srcbot",
            "check-all",
            "nodePackages",
            "/nix/store/nixpkgs",
            "--system",
            "aarch64-linux",
            "--eval-workers",
            "8",
            "--build-jobs",
            "16",
            "--failures-file",
            "/tmp/failures.json",
            "--limit",
            "100",
        ])
        .unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.pkgset, "nodePackages");
            assert_eq!(
                check_args.nixpkgs,
                std::path::PathBuf::from("/nix/store/nixpkgs")
            );
            assert_eq!(check_args.system, "aarch64-linux");
            assert_eq!(check_args.eval_workers, 8);
            assert_eq!(check_args.build_jobs, 16);
            assert_eq!(
                check_args.failures_file,
                Some(std::path::PathBuf::from("/tmp/failures.json"))
            );
            assert_eq!(check_args.limit, Some(100));
        } else {
            panic!("Expected CheckAll command");
        }
    }

    #[test]
    fn test_check_all_with_limit() {
        // Test --limit flag specifically
        let args =
            Args::try_parse_from(["srcbot", "check-all", "python3Packages", ".", "--limit", "50"])
                .unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.limit, Some(50));
        } else {
            panic!("Expected CheckAll command");
        }

        // Test with limit of 1 (edge case)
        let args =
            Args::try_parse_from(["srcbot", "check-all", "python3Packages", ".", "--limit", "1"])
                .unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.limit, Some(1));
        } else {
            panic!("Expected CheckAll command");
        }
    }

    #[test]
    fn test_check_all_missing_args() {
        // Should fail if pkgset or nixpkgs is missing
        assert!(Args::try_parse_from(["srcbot", "check-all"]).is_err());
        assert!(Args::try_parse_from(["srcbot", "check-all", "python3Packages"]).is_err());
    }

    #[test]
    fn test_verify_full_drvs_flag_default_false() {
        // Default (no flag) should be false
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                !verify_args.verify_full_drvs,
                "--verify-full-drvs should default to false"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_verify_full_drvs_flag_true_when_set() {
        // --verify-full-drvs should be true when explicitly set
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-eval",
            "--verify-full-drvs",
            "--prs",
            "12345",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                verify_args.verify_full_drvs,
                "--verify-full-drvs should be true when set"
            );
            assert!(verify_args.full_eval, "--full-eval should be true");
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_verify_full_drvs_with_false_positive() {
        // --verify-full-drvs should work together with --false-positive
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-eval",
            "--verify-full-drvs",
            "--false-positive",
            "--prs",
            "12345",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(verify_args.verify_full_drvs, "--verify-full-drvs should be true");
            assert!(verify_args.false_positive, "--false-positive should be true");
            assert!(verify_args.full_eval, "--full-eval should be true");
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_aggressively_check_fods_default_true() {
        // Default should be true (enabled by default)
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                verify_args.aggressively_check_fods,
                "--aggressively-check-fods should default to true"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_aggressively_check_fods_can_be_disabled() {
        // Should be false when explicitly set to false
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-eval",
            "--aggressively-check-fods=false",
            "--prs",
            "12345",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                !verify_args.aggressively_check_fods,
                "--aggressively-check-fods=false should disable it"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_skip_tests_default_false() {
        // Default should be false (tests are built by default)
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                !verify_args.skip_tests,
                "--skip-tests should default to false"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_skip_tests_flag() {
        // Should be true when explicitly set
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--skip-tests",
            "--prs",
            "12345",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(
                verify_args.skip_tests,
                "--skip-tests should be true when set"
            );
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_remote_builder_defaults() {
        // Remote builder should be None by default
        let args = Args::try_parse_from(["srcbot", "verify", "--prs", "12345"]).unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert!(verify_args.remote_builder.is_none());
            assert!(verify_args.remote_system.is_none());
            assert_eq!(verify_args.remote_build_jobs, 4);
            assert!(verify_args.remote_gc_threshold.is_none());
            assert!(verify_args.remote_gc_keep_days.is_none());
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_remote_builder_basic() {
        // Test basic remote builder specification
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-eval",
            "--prs",
            "12345",
            "--remote-builder",
            "root@150.136.78.22",
            "--remote-system",
            "aarch64-linux",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert_eq!(verify_args.remote_builder, Some("root@150.136.78.22".to_string()));
            assert_eq!(verify_args.remote_system, Some("aarch64-linux".to_string()));
            assert_eq!(verify_args.remote_build_jobs, 4); // default
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_remote_builder_all_options() {
        // Test all remote builder options
        let args = Args::try_parse_from([
            "srcbot",
            "verify",
            "--full-eval",
            "--prs",
            "12345",
            "--remote-builder",
            "user@arm-server",
            "--remote-system",
            "aarch64-linux",
            "--remote-build-jobs",
            "2",
            "--remote-gc-threshold",
            "50G",
            "--remote-gc-keep-days",
            "1",
        ])
        .unwrap();
        if let Commands::Verify(verify_args) = args.command {
            assert_eq!(verify_args.remote_builder, Some("user@arm-server".to_string()));
            assert_eq!(verify_args.remote_system, Some("aarch64-linux".to_string()));
            assert_eq!(verify_args.remote_build_jobs, 2);
            assert_eq!(verify_args.remote_gc_threshold, Some("50G".to_string()));
            assert_eq!(verify_args.remote_gc_keep_days, Some(1));
        } else {
            panic!("Expected Verify command");
        }
    }

    #[test]
    fn test_check_all_remote_builder() {
        // Test remote builder for check-all command
        let args = Args::try_parse_from([
            "srcbot",
            "check-all",
            "python3Packages",
            "/path/to/nixpkgs",
            "--remote-builder",
            "root@arm-host",
            "--remote-system",
            "aarch64-linux",
            "--remote-build-jobs",
            "3",
        ])
        .unwrap();
        if let Commands::CheckAll(check_args) = args.command {
            assert_eq!(check_args.remote_builder, Some("root@arm-host".to_string()));
            assert_eq!(check_args.remote_system, Some("aarch64-linux".to_string()));
            assert_eq!(check_args.remote_build_jobs, 3);
        } else {
            panic!("Expected CheckAll command");
        }
    }

    #[test]
    fn test_fix_hash_defaults() {
        // Test fix-hash command defaults
        let args = Args::try_parse_from([
            "srcbot",
            "fix-hash",
            "--nixpkgs",
            "/path/to/nixpkgs",
            "--attribute",
            "hello",
        ])
        .unwrap();
        if let Commands::FixHash(fix_args) = args.command {
            assert_eq!(fix_args.intermediate, "src");
            assert_eq!(fix_args.system, "x86_64-linux");
            assert!(!fix_args.dont_diff);
            assert!(!fix_args.no_pr_text);
            assert_eq!(fix_args.nix_timeout, 3600);
            assert!(fix_args.remote_builder.is_none());
            assert!(fix_args.remote_system.is_none());
            assert_eq!(fix_args.remote_build_jobs, 4);
        } else {
            panic!("Expected FixHash command");
        }
    }

    #[test]
    fn test_fix_hash_remote_builder() {
        // Test remote builder for fix-hash command
        let args = Args::try_parse_from([
            "srcbot",
            "fix-hash",
            "--nixpkgs",
            "/path/to/nixpkgs",
            "--attribute",
            "python3Packages.requests",
            "--intermediate",
            "src",
            "--remote-builder",
            "root@arm-host",
            "--remote-system",
            "aarch64-linux",
            "--remote-build-jobs",
            "2",
        ])
        .unwrap();
        if let Commands::FixHash(fix_args) = args.command {
            assert_eq!(fix_args.attribute, "python3Packages.requests");
            assert_eq!(fix_args.intermediate, "src");
            assert_eq!(fix_args.remote_builder, Some("root@arm-host".to_string()));
            assert_eq!(fix_args.remote_system, Some("aarch64-linux".to_string()));
            assert_eq!(fix_args.remote_build_jobs, 2);
        } else {
            panic!("Expected FixHash command");
        }
    }
}
