use anyhow::Result;
use std::path::PathBuf;

use crate::commands::run_command_async;

/// Fetch a PR ref from a remote into the local nixpkgs repo
pub async fn fetch_pr_ref(
    nixpkgs_path: &PathBuf,
    remote: &str,
    sha: &str,
    ref_name: &str,
) -> Result<()> {
    let refspec = format!("{}:{}", sha, ref_name);
    run_command_async(
        "git",
        &[
            "-C",
            nixpkgs_path.to_str().unwrap(),
            "-c",
            "fetch.prune=false",
            "fetch",
            "--no-tags",
            "--force",
            remote,
            &refspec,
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_fetch_pr_ref_invalid_path() {
        let invalid_path = PathBuf::from("/nonexistent/path/to/repo");
        let result = fetch_pr_ref(&invalid_path, "origin", "abc123", "refs/pr/123").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_pr_ref_not_a_git_repo() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().to_path_buf();
        let result = fetch_pr_ref(&path, "origin", "abc123", "refs/pr/123").await;
        assert!(result.is_err());
        // Should fail because the temp dir is not a git repo
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not a git repository") || err_msg.contains("failed"),
            "Expected git repo error, got: {}",
            err_msg
        );
    }
}
