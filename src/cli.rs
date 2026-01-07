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
            assert_eq!(verify_args.system, "x86_64-linux");
            assert_eq!(verify_args.build_jobs, 4);
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
}
