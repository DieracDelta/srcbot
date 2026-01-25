use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// GitHub PR info
#[derive(Deserialize, Debug)]
pub struct PullRequest {
    pub title: String,
    pub head: PrRef,
    pub base: PrRef,
    pub merge_commit_sha: Option<String>,
}

/// GitHub PR reference (head or base)
#[derive(Deserialize, Debug)]
pub struct PrRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
}

/// Intermediate attributes to check and build.
/// These are Fixed-Output Derivation (FOD) attributes that should be rebuilt from scratch.
///
/// **Ordering matters:** The `src` attribute must come first because other intermediate
/// attributes (like `goModules`, `cargoDeps`, etc.) may depend on the source being
/// available. The dependency chain is:
///   1. `src` - The source code must be fetched first
///   2. Language-specific deps - These often derive their lockfiles/manifests from `src`
///
/// When building in tiers (like in full_eval.rs), we iterate through this list in order,
/// building all packages' `src` first, then all packages' `goModules`, etc. This ensures
/// dependencies are available when needed.
pub const INTERMEDIATE_ATTRS: &[&str] = &[
    // okay most stuff has this
    "src",
    // Go
    "goModules", // (buildGoModule)
    // Rust
    "cargoDeps", // (buildRustPackage)
    // Node.js
    "npmDeps", // (buildNpmPackage)
    "pnpmDeps",
    "yarnOfflineCache", // (Yarn v1)
    "offlineCache",     // (Yarn Berry)
    // .NET
    "nugetDeps", // .(buildDotnetModule)
    // Dart/Flutter
    "pubcache", // (buildDartApplication, output)
    // PHP
    "vendor",             // (buildComposerWithPlugin)
    "composerVendor",     // (buildComposerProject, new)
    "composerRepository", // (buildComposerProject, old)
    // Java
    "fetchedMavenDeps", // (buildMaven)
    "mitmCache",        // (buildGradlePackage)
    // Elixir
    "mixFodDeps", // (buildMixRelease)
];

/// A package whose intermediates or final drvPath changed between base and PR
#[derive(Debug, Clone)]
pub struct ChangedPackage {
    /// Attr path
    pub attr: String,
    /// Which intermediate attrs changed (e.g., ["src", "goModules"])
    pub changed_intermediates: Vec<String>,
    /// The drvPaths for each changed intermediate (PR version) - kept for potential future use
    #[allow(dead_code)]
    pub intermediate_drv_paths: HashMap<String, String>,
    /// True if the final package drvPath changed (but intermediates didn't)
    /// Only populated when --verify-full-drvs is used
    pub final_drv_changed: bool,
}

/// Result of building a package in full-eval mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullEvalBuildResult {
    pub attr: String,
    /// The system/architecture this result is for (e.g., "x86_64-linux", "aarch64-linux")
    #[serde(default)]
    pub system: String,
    pub intermediate_results: Vec<(String, bool, String)>, // (name, success, logs)
    pub package_success: bool,
    pub package_logs: String,
    /// True if the failure also exists on the base branch (pre-existing failure)
    #[serde(default)]
    pub is_false_positive: bool,
    /// True if any intermediate or final build was non-deterministic (hash mismatch on --check)
    #[serde(default)]
    pub is_non_deterministic: bool,
}

/// Serializable version of ChangedPackage for state persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedPackageSer {
    pub attr: String,
    pub changed_intermediates: Vec<String>,
    /// True if the final package drvPath changed (but intermediates didn't)
    #[serde(default)]
    pub final_drv_changed: bool,
}

/// Configuration for a remote builder
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBuilderConfig {
    /// SSH connection string (e.g., "root@host" or "user@192.168.1.1")
    pub ssh_target: String,
    /// Target system architecture (e.g., "aarch64-linux")
    pub system: String,
    /// Maximum parallel jobs on this builder
    pub max_jobs: usize,
    /// Disk usage threshold for triggering GC (e.g., "50G")
    pub gc_threshold: Option<String>,
    /// Keep builds newer than N days during GC
    pub gc_keep_days: Option<u64>,
}

impl RemoteBuilderConfig {
    /// Format as Nix --builders string
    /// Format: ssh://user@host system ssh-key max-jobs speed-factor features
    pub fn to_builders_arg(&self) -> String {
        format!(
            "ssh://{} {} - {} 1 big-parallel",
            self.ssh_target,
            self.system,
            self.max_jobs
        )
    }
}

/// Saved state for resuming a run
/// Note: When multi-arch support is fully implemented, this will be extended to
/// support per-system state (systems, packages_to_build per system, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub pr_num: u64,
    pub merge_base: String,
    pub pr_head_sha: String,
    pub system: String,
    /// The CLI command that was run (for COMMAND.md and summary)
    #[serde(default)]
    pub cli_command: String,
    /// All packages that need to be built
    pub packages_to_build: Vec<ChangedPackageSer>,
    /// Intermediate build results per package: attr -> [(intermediate_name, success, logs)]
    pub intermediate_results: HashMap<String, Vec<(String, bool, String)>>,
    /// Results for packages that have been fully completed (including final build)
    pub completed_results: Vec<FullEvalBuildResult>,
    /// Whether intermediate builds have been posted to GitHub
    pub intermediates_posted: bool,
}

/// Result of building a single attribute (used in simple mode)
pub struct AttrBuildResult {
    /// The sub-attribute name (e.g., "src", "goModules")
    pub name: String,
    /// Whether the attribute exists on the package
    pub exists: bool,
    /// Exit code from the build (None if not attempted or signal)
    pub code: Option<i32>,
    /// Build logs
    pub logs: String,
    /// Command that was run
    pub cmd: String,
}

impl AttrBuildResult {
    pub fn success(&self) -> bool {
        !self.exists || self.code == Some(0)
    }

    pub fn attempted(&self) -> bool {
        self.exists
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intermediate_attrs_src_is_first() {
        // CRITICAL: src must be first because other intermediate attrs
        // (goModules, cargoDeps, etc.) may depend on src being available
        assert_eq!(
            INTERMEDIATE_ATTRS[0], "src",
            "src must be the first intermediate attr for dependency ordering"
        );
    }

    #[test]
    fn test_intermediate_attrs_has_expected_count() {
        // Ensure we don't accidentally remove attrs
        assert!(
            INTERMEDIATE_ATTRS.len() >= 15,
            "Expected at least 15 intermediate attrs, found {}",
            INTERMEDIATE_ATTRS.len()
        );
    }

    #[test]
    fn test_intermediate_attrs_contains_major_languages() {
        let expected = vec![
            "src",       // universal
            "goModules", // Go
            "cargoDeps", // Rust
            "npmDeps",   // Node.js
            "nugetDeps", // .NET
        ];
        for attr in expected {
            assert!(
                INTERMEDIATE_ATTRS.contains(&attr),
                "INTERMEDIATE_ATTRS should contain {}",
                attr
            );
        }
    }

    #[test]
    fn test_pull_request_deserialize() {
        let json = r#"{
            "title": "test pr",
            "head": {"sha": "abc123", "ref": "feature-branch"},
            "base": {"sha": "def456", "ref": "master"},
            "merge_commit_sha": "ghi789"
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.title, "test pr");
        assert_eq!(pr.head.sha, "abc123");
        assert_eq!(pr.head.ref_name, "feature-branch");
        assert_eq!(pr.base.sha, "def456");
        assert_eq!(pr.base.ref_name, "master");
        assert_eq!(pr.merge_commit_sha, Some("ghi789".to_string()));
    }

    #[test]
    fn test_pull_request_deserialize_no_merge_commit() {
        let json = r#"{
            "title": "draft pr",
            "head": {"sha": "abc123", "ref": "draft"},
            "base": {"sha": "def456", "ref": "main"},
            "merge_commit_sha": null
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.title, "draft pr");
        assert!(pr.merge_commit_sha.is_none());
    }

    #[test]
    fn test_full_eval_build_result_serialize_deserialize() {
        let result = FullEvalBuildResult {
            attr: "hello".to_string(),
            system: "x86_64-linux".to_string(),
            intermediate_results: vec![
                ("src".to_string(), true, "ok".to_string()),
                ("goModules".to_string(), false, "hash mismatch".to_string()),
            ],
            package_success: false,
            package_logs: "failed".to_string(),
            is_false_positive: false,
            is_non_deterministic: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: FullEvalBuildResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.attr, "hello");
        assert_eq!(deserialized.system, "x86_64-linux");
        assert_eq!(deserialized.intermediate_results.len(), 2);
        assert!(!deserialized.package_success);
        assert!(!deserialized.is_false_positive);
        assert!(!deserialized.is_non_deterministic);
    }

    #[test]
    fn test_full_eval_build_result_false_positive() {
        let result = FullEvalBuildResult {
            attr: "broken-pkg".to_string(),
            system: "aarch64-linux".to_string(),
            intermediate_results: vec![("src".to_string(), false, "404".to_string())],
            package_success: false,
            package_logs: "".to_string(),
            is_false_positive: true,
            is_non_deterministic: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: FullEvalBuildResult = serde_json::from_str(&json).unwrap();
        assert!(deserialized.is_false_positive);
    }

    #[test]
    fn test_full_eval_build_result_backwards_compat() {
        // Test that old serialized data without is_false_positive field works
        let json = r#"{
            "attr": "old-pkg",
            "intermediate_results": [["src", true, "ok"]],
            "package_success": true,
            "package_logs": ""
        }"#;
        let deserialized: FullEvalBuildResult = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.attr, "old-pkg");
        assert!(!deserialized.is_false_positive); // defaults to false
    }

    #[test]
    fn test_run_state_serialize_deserialize() {
        let state = RunState {
            pr_num: 12345,
            merge_base: "abc123".to_string(),
            pr_head_sha: "def456".to_string(),
            system: "x86_64-linux".to_string(),
            cli_command: "srcbot verify --full-eval --prs 12345".to_string(),
            packages_to_build: vec![ChangedPackageSer {
                attr: "hello".to_string(),
                changed_intermediates: vec!["src".to_string()],
                final_drv_changed: false,
            }],
            intermediate_results: HashMap::new(),
            completed_results: vec![],
            intermediates_posted: false,
        };
        let json = serde_json::to_string(&state).unwrap();
        let deserialized: RunState = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.pr_num, 12345);
        assert_eq!(deserialized.system, "x86_64-linux");
        assert_eq!(deserialized.cli_command, "srcbot verify --full-eval --prs 12345");
        assert_eq!(deserialized.packages_to_build.len(), 1);
    }

    #[test]
    fn test_run_state_backwards_compat() {
        // Test that old serialized data without cli_command field works
        let json = r#"{
            "pr_num": 12345,
            "merge_base": "abc123",
            "pr_head_sha": "def456",
            "system": "x86_64-linux",
            "packages_to_build": [],
            "intermediate_results": {},
            "completed_results": [],
            "intermediates_posted": false
        }"#;
        let deserialized: RunState = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.pr_num, 12345);
        assert_eq!(deserialized.cli_command, ""); // defaults to empty
    }

    #[test]
    fn test_changed_package_ser_serialize_deserialize() {
        let pkg = ChangedPackageSer {
            attr: "myPackage".to_string(),
            changed_intermediates: vec!["src".to_string(), "cargoDeps".to_string()],
            final_drv_changed: false,
        };
        let json = serde_json::to_string(&pkg).unwrap();
        let deserialized: ChangedPackageSer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.attr, "myPackage");
        assert_eq!(deserialized.changed_intermediates.len(), 2);
        assert!(!deserialized.final_drv_changed);
    }

    #[test]
    fn test_changed_package_ser_final_drv_changed() {
        let pkg = ChangedPackageSer {
            attr: "finalOnlyPkg".to_string(),
            changed_intermediates: vec![],
            final_drv_changed: true,
        };
        let json = serde_json::to_string(&pkg).unwrap();
        let deserialized: ChangedPackageSer = serde_json::from_str(&json).unwrap();
        assert!(deserialized.final_drv_changed);
        assert!(deserialized.changed_intermediates.is_empty());
    }

    #[test]
    fn test_changed_package_ser_backwards_compat() {
        // Test that old serialized data without final_drv_changed field works
        let json = r#"{
            "attr": "old-pkg",
            "changed_intermediates": ["src"]
        }"#;
        let deserialized: ChangedPackageSer = serde_json::from_str(json).unwrap();
        assert_eq!(deserialized.attr, "old-pkg");
        assert!(!deserialized.final_drv_changed); // defaults to false
    }

    #[test]
    fn test_remote_builder_config_to_builders_arg() {
        let config = RemoteBuilderConfig {
            ssh_target: "root@150.136.78.22".to_string(),
            system: "aarch64-linux".to_string(),
            max_jobs: 4,
            gc_threshold: Some("50G".to_string()),
            gc_keep_days: Some(1),
        };
        let builders_arg = config.to_builders_arg();
        assert_eq!(
            builders_arg,
            "ssh://root@150.136.78.22 aarch64-linux - 4 1 big-parallel"
        );
    }

    #[test]
    fn test_remote_builder_config_serialize_deserialize() {
        let config = RemoteBuilderConfig {
            ssh_target: "user@arm-server".to_string(),
            system: "aarch64-linux".to_string(),
            max_jobs: 2,
            gc_threshold: None,
            gc_keep_days: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: RemoteBuilderConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.ssh_target, "user@arm-server");
        assert_eq!(deserialized.system, "aarch64-linux");
        assert_eq!(deserialized.max_jobs, 2);
        assert!(deserialized.gc_threshold.is_none());
    }
}
