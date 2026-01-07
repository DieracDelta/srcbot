use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as TokioCommand;
use tracing::{error, info, warn};
use tempfile::TempDir;

use crate::cache::get_fix_hash_attr_log_dir;
use crate::commands::run_command_async;
use crate::nix::{has_attr, was_built_locally};

/// Result of parsing a hash mismatch from nix build output
#[derive(Debug, Clone)]
pub struct HashMismatch {
    /// The hash that was specified in the Nix expression
    pub specified: String,
    /// The hash that was actually computed
    pub got: String,
}

/// Parse hash mismatch from nix build output
/// Looks for patterns like:
///   hash mismatch in fixed-output derivation '/nix/store/...':
///     specified: sha256-...
///     got:       sha256-...
fn parse_hash_mismatch(output: &str) -> Option<HashMismatch> {
    // Pattern for the newer nix output format
    let re = Regex::new(r"specified:\s+(sha256-[a-zA-Z0-9+/=]+)\s+got:\s+(sha256-[a-zA-Z0-9+/=]+)").ok()?;

    if let Some(caps) = re.captures(output) {
        return Some(HashMismatch {
            specified: caps.get(1)?.as_str().to_string(),
            got: caps.get(2)?.as_str().to_string(),
        });
    }

    // Alternative pattern with different whitespace
    let re2 = Regex::new(r"(?s)specified:\s*(sha256-[a-zA-Z0-9+/=]+).*?got:\s*(sha256-[a-zA-Z0-9+/=]+)").ok()?;
    if let Some(caps) = re2.captures(output) {
        return Some(HashMismatch {
            specified: caps.get(1)?.as_str().to_string(),
            got: caps.get(2)?.as_str().to_string(),
        });
    }

    // Pattern for SRI hashes with different algorithms
    let re3 = Regex::new(r"specified:\s+(sha\d+-[a-zA-Z0-9+/=]+)\s+got:\s+(sha\d+-[a-zA-Z0-9+/=]+)").ok()?;
    if let Some(caps) = re3.captures(output) {
        return Some(HashMismatch {
            specified: caps.get(1)?.as_str().to_string(),
            got: caps.get(2)?.as_str().to_string(),
        });
    }

    None
}

/// Delete the store path for an intermediate attribute to force rebuild
async fn gc_intermediate_store_path(nixpkgs_path: &PathBuf, attr: &str, intermediate: &str, system: &str) -> Result<()> {
    let full_attr = format!("{}.{}", attr, intermediate);
    let expr = format!(
        "with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}.outPath",
        nixpkgs_path.display(),
        system,
        full_attr
    );

    // Get the store path
    let result = run_command_async("nix-instantiate", &["--eval", "--expr", &expr]).await;
    if let Ok(output) = result {
        let store_path = output.trim().trim_matches('"');
        if store_path.starts_with("/nix/store/") && std::path::Path::new(store_path).exists() {
            info!("Deleting store path to force rebuild: {}", store_path);
            let delete_result = TokioCommand::new("nix-store")
                .args(["--delete", store_path])
                .output()
                .await;
            match delete_result {
                Ok(output) if output.status.success() => {
                    info!("Successfully deleted {}", store_path);
                }
                Ok(output) => {
                    // Deletion might fail if path is still referenced, that's okay
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!("Could not delete store path (may be in use): {}", stderr.trim());
                }
                Err(e) => {
                    warn!("Failed to run nix-store --delete: {}", e);
                }
            }
        }
    }
    Ok(())
}

/// Build an intermediate attribute and capture the output
/// Returns (success, output_logs, store_path_if_success)
/// log_suffix: e.g., "before" or "after" to distinguish build phases
async fn build_intermediate(
    nixpkgs_path: &PathBuf,
    attr: &str,
    intermediate: &str,
    system: &str,
    log_suffix: &str,
) -> Result<(bool, String, Option<String>)> {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    let full_attr = format!("{}.{}", attr, intermediate);
    let expr = format!(
        "with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}",
        nixpkgs_path.display(),
        system,
        full_attr
    );

    info!("Building {}", full_attr);

    // Set up log file: {save-location}/logs/{attr}/{intermediate}.{suffix}.log
    let log_dir = get_fix_hash_attr_log_dir(attr)?;
    let log_path = log_dir.join(format!("{}.{}.log", intermediate, log_suffix));

    info!("Streaming build logs to: {}", log_path.display());

    let log_file = std::fs::File::create(&log_path)
        .with_context(|| format!("Failed to create log file {}", log_path.display()))?;
    let log_file = Arc::new(Mutex::new(log_file));

    // Check if store path exists to decide whether to use --check
    let path_expr = format!("{}.outPath", expr);
    let (store_path_exists, output_path) = match TokioCommand::new("nix-instantiate")
        .args(["--eval", "--expr", &path_expr])
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let path = String::from_utf8_lossy(&output.stdout).trim().trim_matches('"').to_string();
            let exists = path.starts_with("/nix/store/") && std::path::Path::new(&path).exists();
            (exists, Some(path))
        },
        _ => (false, None),
    };

    // Track if path existed before build (for post-build verification)
    let existed_before = store_path_exists;

    // Build command args:
    // - If store path exists, use --check to verify it matches (re-fetches and compares)
    // - If not:
    //   - "before": force fresh fetch with --substituters "" (to trigger hash mismatch)
    //   - "after": cache-friendly build (can use substituters), then verify if from cache
    let is_after_build = log_suffix != "before";
    let using_cache_friendly_build = is_after_build && !store_path_exists;
    let args: Vec<&str> = if store_path_exists {
        // Path exists - use --check to verify
        vec!["--no-out-link", "--check", "--expr", &expr]
    } else if is_after_build {
        // "after" build: cache-friendly mode
        vec!["--no-out-link", "--expr", &expr]
    } else {
        // "before" build: force fresh fetch to trigger hash mismatch
        vec!["--no-out-link", "--substituters", "", "--expr", &expr]
    };

    // Print the command being run
    let cmd_str = format!("nix-build {}", args.join(" "));
    info!("Running: {}", cmd_str);
    println!("$ {}", cmd_str);

    let mut child = TokioCommand::new("nix-build")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-build")?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let logs = Arc::new(Mutex::new(Vec::new()));
    let store_path = Arc::new(Mutex::new(None));

    // Read stdout
    let logs_clone = logs.clone();
    let log_file_clone = log_file.clone();
    let store_path_clone = store_path.clone();
    let stdout_task = tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // nix-build outputs the store path on success
                if line.starts_with("/nix/store/") {
                    if let Ok(mut sp) = store_path_clone.lock() {
                        *sp = Some(line.clone());
                    }
                }
                // Write to log file immediately
                if let Ok(mut file) = log_file_clone.lock() {
                    let _ = writeln!(file, "{}", line);
                    let _ = file.flush();
                }
                // Also print to console
                println!("{}", line);
                // Collect for return value
                if let Ok(mut log_guard) = logs_clone.lock() {
                    log_guard.push(line);
                }
            }
        }
    });

    // Read stderr
    let logs_clone = logs.clone();
    let log_file_clone = log_file.clone();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Write to log file immediately
                if let Ok(mut file) = log_file_clone.lock() {
                    let _ = writeln!(file, "{}", line);
                    let _ = file.flush();
                }
                // Also print to console (stderr)
                eprintln!("{}", line);
                // Collect for return value
                if let Ok(mut log_guard) = logs_clone.lock() {
                    log_guard.push(line);
                }
            }
        }
    });

    // Wait for process and output readers
    let (status, _, _) = tokio::join!(child.wait(), stdout_task, stderr_task);

    let success = status.map(|s| s.success()).unwrap_or(false);
    let mut log_text = logs.lock().map(|l| l.join("\n")).unwrap_or_default();
    let final_store_path = store_path.lock().ok().and_then(|sp| sp.clone());

    // For cache-friendly builds that succeeded, verify with --check if the path was from cache
    // This handles three scenarios:
    // 1. Path existed before build → already used --check above
    // 2. Path didn't exist, pulled from cache → need to run --check now
    // 3. Path didn't exist, built fresh → skip --check
    if using_cache_friendly_build && success {
        if let Some(ref path) = output_path {
            // Determine if we need to run --check:
            // - If path existed before build → already handled above with --check
            // - If path didn't exist but was pulled from cache → verify now
            // - If path didn't exist and was built fresh → skip
            let needs_check = if existed_before {
                // Already ran --check above
                false
            } else {
                // Check if it was pulled from cache vs built fresh
                let built_locally = was_built_locally(path).await.unwrap_or(false);
                if !built_locally {
                    info!("{} was from cache, verifying with --check...", full_attr);
                    true
                } else {
                    info!("{} was built fresh, skipping --check", full_attr);
                    false
                }
            };

            if needs_check {
                log_text.push_str("\n--- Running --check to verify FOD ---\n");

                let check_args = vec!["--no-out-link", "--check", "--expr", &expr];
                match TokioCommand::new("nix-build")
                    .args(&check_args)
                    .output()
                    .await
                {
                    Ok(output) => {
                        let check_success = output.status.success();
                        let check_stdout = String::from_utf8_lossy(&output.stdout);
                        let check_stderr = String::from_utf8_lossy(&output.stderr);
                        log_text.push_str(&check_stdout);
                        log_text.push_str(&check_stderr);

                        // Write check logs to file
                        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                            use std::io::Write;
                            let _ = writeln!(file, "\n--- --check verification ---");
                            let _ = write!(file, "{}", check_stdout);
                            let _ = write!(file, "{}", check_stderr);
                        }

                        if !check_success {
                            return Ok((false, log_text, final_store_path));
                        }
                    }
                    Err(e) => {
                        log_text.push_str(&format!("Failed to run --check: {}", e));
                        return Ok((false, log_text, final_store_path));
                    }
                }
            }
        }
    }

    Ok((success, log_text, final_store_path))
}

/// Find the Nix file that defines an attribute using nix-instantiate --eval
async fn find_nix_file_for_attr(nixpkgs_path: &PathBuf, attr: &str, system: &str) -> Result<PathBuf> {
    // Use builtins.unsafeGetAttrPos to find the file
    let expr = format!(
        r#"let pkgs = import {} {{ system = "{}"; }}; in (builtins.unsafeGetAttrPos "version" pkgs.{}).file or (builtins.unsafeGetAttrPos "pname" pkgs.{}).file or (builtins.unsafeGetAttrPos "name" pkgs.{}).file"#,
        nixpkgs_path.display(),
        system,
        attr,
        attr,
        attr
    );

    match run_command_async("nix-instantiate", &["--eval", "--expr", &expr]).await {
        Ok(result) => {
            let path = result.trim().trim_matches('"');
            Ok(PathBuf::from(path))
        }
        Err(_) => {
            // Fallback: try to find the file manually
            Err(anyhow!("Could not find Nix file for attribute {}", attr))
        }
    }
}

/// Update a hash in a Nix file
/// This is a best-effort approach - it searches for the old hash and replaces it
fn update_hash_in_file(file_path: &PathBuf, old_hash: &str, new_hash: &str) -> Result<bool> {
    let content = std::fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read {}", file_path.display()))?;

    if !content.contains(old_hash) {
        warn!("Old hash {} not found in {}", old_hash, file_path.display());
        return Ok(false);
    }

    let new_content = content.replace(old_hash, new_hash);
    std::fs::write(file_path, &new_content)
        .with_context(|| format!("Failed to write {}", file_path.display()))?;

    info!("Updated hash in {}: {} -> {}", file_path.display(), old_hash, new_hash);
    Ok(true)
}

/// Run difftastic on two directories/files and capture the output
async fn run_difftastic(old_path: &str, new_path: &str) -> Result<String> {
    let output = TokioCommand::new("difft")
        .args(["--color", "never", "--skip-unchanged", old_path, new_path])
        .output()
        .await
        .context("Failed to run difftastic (difft). Is it installed?")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    Ok(format!("{}{}", stdout, stderr))
}

/// Result of a successful hash fix operation
#[derive(Debug, Clone)]
pub struct FixHashResult {
    pub attribute: String,
    pub intermediate: String,
    pub before_commit: String,
    pub after_commit: String,
    pub has_diff: bool,
    pub diff_failed_reason: Option<String>,
    pub has_old_source: bool,
    pub has_new_source: bool,
    pub before_log: Option<String>,
    pub after_log: Option<String>,
    pub diff_content: Option<String>,
}

/// Copy a nix store path to a destination directory
async fn copy_source_to_logs(store_path: &str, dest_dir: &PathBuf) -> Result<()> {
    use std::fs;

    // Remove dest if it exists
    if dest_dir.exists() {
        fs::remove_dir_all(dest_dir)?;
    }

    // Use cp -rL to dereference symlinks (nix store paths are often symlinked)
    let status = TokioCommand::new("cp")
        .args(["-rL", store_path, dest_dir.to_str().unwrap()])
        .status()
        .await
        .context("Failed to run cp")?;

    if !status.success() {
        return Err(anyhow!("cp command failed"));
    }

    // Make the copied files readable (nix store is read-only)
    let _ = TokioCommand::new("chmod")
        .args(["-R", "u+rw", dest_dir.to_str().unwrap()])
        .status()
        .await;

    Ok(())
}

/// Try to fetch the old source from substituters (cache)
/// This gets the store path for the current (old) hash and tries to realise it from cache
async fn fetch_from_substituters(
    nixpkgs_path: &PathBuf,
    attr: &str,
    intermediate: &str,
    system: &str,
) -> Result<Option<String>> {
    let full_attr = format!("{}.{}", attr, intermediate);

    // First, get the store path for the old hash
    let path_expr = format!(
        "with import {} {{ system = \"{}\"; config = {{ allowUnfree = true; }}; }}; {}.outPath",
        nixpkgs_path.display(),
        system,
        full_attr
    );

    info!("Getting store path for old source...");
    let cmd_str = format!("nix-instantiate --eval --expr '{}'", path_expr);
    info!("Running: {}", cmd_str);
    println!("$ {}", cmd_str);

    let path_output = TokioCommand::new("nix-instantiate")
        .args(["--eval", "--expr", &path_expr])
        .output()
        .await
        .context("Failed to get store path")?;

    if !path_output.status.success() {
        info!("Could not determine store path for old source");
        return Ok(None);
    }

    let store_path = String::from_utf8_lossy(&path_output.stdout)
        .trim()
        .trim_matches('"')
        .to_string();

    if !store_path.starts_with("/nix/store/") {
        info!("Invalid store path: {}", store_path);
        return Ok(None);
    }

    info!("Old source store path: {}", store_path);

    // Check if it already exists locally
    if std::path::Path::new(&store_path).exists() {
        info!("Old source already exists locally: {}", store_path);
        return Ok(Some(store_path));
    }

    // Try to fetch from substituters using nix-store --realise
    info!("Trying to fetch old source from substituters...");
    let cmd_str = format!("nix-store --realise {}", store_path);
    info!("Running: {}", cmd_str);
    println!("$ {}", cmd_str);

    let output = TokioCommand::new("nix-store")
        .args(["--realise", &store_path])
        .output()
        .await
        .context("Failed to run nix-store --realise")?;

    if output.status.success() {
        info!("Fetched old source from cache: {}", store_path);
        return Ok(Some(store_path));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    info!("Could not fetch old source from substituters: {}", stderr.trim());
    Ok(None)
}

/// Generate PR text for a hash fix (nixpkgs format)
fn generate_pr_text(result: &FixHashResult, log_base_url: &str) -> String {
    let title = format!("{}: fix {} hash", result.attribute, result.intermediate);

    let attr_safe = result.attribute.replace('.', "_").replace('/', "_");
    let before_log_url = format!("{}/logs/{}/{}.before.log", log_base_url, attr_safe, result.intermediate);
    let after_log_url = format!("{}/logs/{}/{}.after.log", log_base_url, attr_safe, result.intermediate);

    // Short commit hashes (first 7 chars)
    let before_short = result.before_commit.chars().take(7).collect::<String>();
    let after_short = result.after_commit.chars().take(7).collect::<String>();

    let mut body = format!(
        r#"Fixes hash mismatch for `{attr}.{intermediate}`.

Build logs:
- [before ({before_commit})]({before_url})
- [after ({after_commit})]({after_url})"#,
        attr = result.attribute,
        intermediate = result.intermediate,
        before_commit = before_short,
        after_commit = after_short,
        before_url = before_log_url,
        after_url = after_log_url
    );

    if result.has_diff {
        let diff_log_url = format!("{}/logs/{}/{}.diff.log", log_base_url, attr_safe, result.intermediate);
        body.push_str(&format!("\n- [source diff]({})", diff_log_url));
    } else if let Some(ref reason) = result.diff_failed_reason {
        body.push_str(&format!("\n\n*Source diff not available: {}*", reason));
    }

    // Add source directory links
    if result.has_old_source || result.has_new_source {
        body.push_str("\n\nSource directories:");
        if result.has_old_source {
            let old_src_url = format!("{}/logs/{}/{}.src.before/", log_base_url, attr_safe, result.intermediate);
            body.push_str(&format!("\n- [old source]({})", old_src_url));
        }
        if result.has_new_source {
            let new_src_url = format!("{}/logs/{}/{}.src.after/", log_base_url, attr_safe, result.intermediate);
            body.push_str(&format!("\n- [new source]({})", new_src_url));
        }
    }

    // Add dropdowns with log contents
    if let Some(ref diff) = result.diff_content {
        let diff_lines: Vec<&str> = diff.lines().take(100).collect();
        let truncated = if diff.lines().count() > 100 { " (first 100 lines)" } else { "" };
        body.push_str(&format!(r#"

<details>
<summary>Source diff{}</summary>

```
{}
```
</details>"#, truncated, diff_lines.join("\n")));
    }

    if let Some(ref before) = result.before_log {
        let line_count = before.lines().count();
        let (before_lines, truncated) = if line_count > 100 {
            let last_100: Vec<&str> = before.lines().rev().take(100).collect();
            let reversed: Vec<&str> = last_100.into_iter().rev().collect();
            (reversed, " (last 100 lines)")
        } else {
            (before.lines().collect(), "")
        };
        body.push_str(&format!(r#"

<details>
<summary>Build before{}</summary>

```
{}
```
</details>"#, truncated, before_lines.join("\n")));
    }

    if let Some(ref after) = result.after_log {
        let line_count = after.lines().count();
        let (after_lines, truncated) = if line_count > 100 {
            let last_100: Vec<&str> = after.lines().rev().take(100).collect();
            let reversed: Vec<&str> = last_100.into_iter().rev().collect();
            (reversed, " (last 100 lines)")
        } else {
            (after.lines().collect(), "")
        };
        body.push_str(&format!(r#"

<details>
<summary>Build after{}</summary>

```
{}
```
</details>"#, truncated, after_lines.join("\n")));
    }

    body.push_str(r#"

Partially addresses #471498

Partially supercedes #471221

<!-- Please check what applies. Note that these are not hard requirements but merely serve as information for reviewers. -->

## Things done

- Built on platform:
  - [x] x86_64-linux
  - [ ] aarch64-linux
  - [ ] x86_64-darwin
  - [ ] aarch64-darwin
- Tested, as applicable:
  - [ ] [NixOS tests] in [nixos/tests].
  - [ ] [Package tests] at `passthru.tests`.
  - [ ] Tests in [lib/tests] or [pkgs/test] for functions and "core" functionality.
- [ ] Ran `nixpkgs-review` on this PR. See [nixpkgs-review usage].
- [ ] Tested basic functionality of all binary files, usually in `./result/bin/`.
- Nixpkgs Release Notes
  - [ ] Package update: when the change is major or breaking.
- NixOS Release Notes
  - [ ] Module addition: when adding a new NixOS module.
  - [ ] Module update: when the change is significant.
- [ ] Fits [CONTRIBUTING.md], [pkgs/README.md], [maintainers/README.md] and other READMEs.

[NixOS tests]: https://nixos.org/manual/nixos/unstable/index.html#sec-nixos-tests
[Package tests]: https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md#package-tests
[nixpkgs-review usage]: https://github.com/Mic92/nixpkgs-review#usage

[CONTRIBUTING.md]: https://github.com/NixOS/nixpkgs/blob/master/CONTRIBUTING.md
[lib/tests]: https://github.com/NixOS/nixpkgs/blob/master/lib/tests
[maintainers/README.md]: https://github.com/NixOS/nixpkgs/blob/master/maintainers/README.md
[nixos/tests]: https://github.com/NixOS/nixpkgs/blob/master/nixos/tests
[pkgs/README.md]: https://github.com/NixOS/nixpkgs/blob/master/pkgs/README.md
[pkgs/test]: https://github.com/NixOS/nixpkgs/blob/master/pkgs/test

---

Add a :+1: [reaction] to [pull requests you find important].

[reaction]: https://github.blog/2016-03-10-add-reactions-to-pull-requests-issues-and-comments/
[pull requests you find important]: https://github.com/NixOS/nixpkgs/pulls?q=is%3Aopen+sort%3Areactions-%2B1-desc
"#);

    format!("# {}\n\n{}", title, body)
}

/// Create a git branch, commit changes, and push to remote
/// Returns the new commit hash on success
async fn git_commit_and_push(
    nixpkgs_path: &PathBuf,
    attribute: &str,
    intermediate: &str,
    old_hash: &str,
    new_hash: &str,
    nix_file: &PathBuf,
    origin: &str,
    branch: &str,
) -> Result<String> {
    let nixpkgs_str = nixpkgs_path.to_str().unwrap();

    // Create and checkout the branch
    info!("Creating branch: {}", branch);

    // Delete the branch if it exists (locally and remotely)
    let _ = run_command_async("git", &["-C", nixpkgs_str, "branch", "-D", branch]).await;

    run_command_async("git", &["-C", nixpkgs_str, "checkout", "-b", branch])
        .await
        .context("Failed to create branch")?;

    // Stage the modified file
    let nix_file_rel = nix_file.strip_prefix(nixpkgs_path)
        .unwrap_or(nix_file);

    run_command_async("git", &[
        "-C", nixpkgs_str,
        "add",
        nix_file_rel.to_str().unwrap()
    ])
    .await
    .context("Failed to stage file")?;

    // Create commit message
    let commit_msg = format!(
        "{}: fix {} hash\n\n{} -> {}",
        attribute, intermediate, old_hash, new_hash
    );

    run_command_async("git", &[
        "-C", nixpkgs_str,
        "commit",
        "-m", &commit_msg
    ])
    .await
    .context("Failed to commit")?;

    // Get the new commit hash
    let new_commit = run_command_async("git", &[
        "-C", nixpkgs_str,
        "rev-parse", "HEAD"
    ])
    .await
    .context("Failed to get new commit hash")?;

    // Push to remote
    info!("Pushing to {}/{}", origin, branch);
    run_command_async("git", &[
        "-C", nixpkgs_str,
        "push", "-f",
        origin,
        &format!("{}:{}", branch, branch)
    ])
    .await
    .context("Failed to push to remote")?;

    Ok(new_commit.trim().to_string())
}

/// Inner function to process fix hash using a prepared worktree
async fn process_fix_hash_worktree(
    nixpkgs_path: &PathBuf,
    attribute: &str,
    intermediate: &str,
    system: &str,
    dont_diff: bool,
    log_location: Option<&PathBuf>,
    origin: Option<&String>,
    branch: Option<&String>,
    log_base_url: &str,
    no_pr_text: bool,
) -> Result<bool> {
    info!("==========================================");
    info!("Fix Hash Worktree: {} ({})", attribute, intermediate);
    info!("==========================================");

    // Get the current commit hash
    let nixpkgs_commit = run_command_async("git", &[
        "-C", nixpkgs_path.to_str().unwrap(),
        "rev-parse", "HEAD"
    ])
    .await
    .context("Failed to get nixpkgs commit")?;
    info!("Nixpkgs commit: {}", nixpkgs_commit);

    // Verify the attribute exists
    if !has_attr(nixpkgs_path, attribute, intermediate, system).await {
        error!("Attribute {}.{} does not exist", attribute, intermediate);
        return Ok(false);
    }

    // Step 1.5: GC the store path to force a fresh build
    info!("Step 1.5: Deleting local store path to force rebuild...");
    gc_intermediate_store_path(nixpkgs_path, attribute, intermediate, system).await?;

    // Step 2: Try to build the attribute (expect hash mismatch)
    info!("Step 2: Building {}.{} to detect hash mismatch...", attribute, intermediate);

    let (success, build_logs, _old_store_path) = build_intermediate(
        nixpkgs_path,
        attribute,
        intermediate,
        system,
        "before",
    ).await?;

    if success {
        info!("Build succeeded - no hash mismatch detected!");
        return Ok(true);
    }

    // Step 3: Parse the hash mismatch from the output
    info!("Step 3: Parsing hash mismatch from build output...");

    let mismatch = parse_hash_mismatch(&build_logs)
        .ok_or_else(|| anyhow!("Could not parse hash mismatch from build output. Output:\n{}", build_logs))?;

    info!("Found hash mismatch:");
    info!("  Specified: {}", mismatch.specified);
    info!("  Got:       {}", mismatch.got);

    // Step 3.5: Try to fetch old source from substituters BEFORE updating the hash
    info!("Step 3.5: Trying to fetch old source from substituters...");
    let old_source_path = fetch_from_substituters(nixpkgs_path, attribute, intermediate, system).await?;

    // Copy old source to logs if available
    let mut has_old_source = false;
    if let Some(ref old_path) = old_source_path {
        let log_dir = get_fix_hash_attr_log_dir(attribute)?;
        let old_src_dest = log_dir.join(format!("{}.src.before", intermediate));
        match copy_source_to_logs(old_path, &old_src_dest).await {
            Ok(()) => {
                info!("Copied old source to: {}", old_src_dest.display());
                has_old_source = true;
            }
            Err(e) => {
                warn!("Failed to copy old source: {}", e);
            }
        }
    }

    // Step 4: Find the Nix file and update the hash
    info!("Step 4: Finding and updating Nix file...");

    let nix_file = find_nix_file_for_attr(nixpkgs_path, attribute, system).await?;
    info!("Found Nix file: {}", nix_file.display());

    let mut updated_file = nix_file.clone();
    let updated = update_hash_in_file(&nix_file, &mismatch.specified, &mismatch.got)?;
    if !updated {
        // Try searching in the nixpkgs directory for the hash
        warn!("Hash not found in main file, searching nixpkgs directory...");

        let search_result = TokioCommand::new("grep")
            .args(["-r", "-l", &mismatch.specified])
            .current_dir(nixpkgs_path)
            .output()
            .await
            .context("Failed to search for hash in nixpkgs")?;

        let files = String::from_utf8_lossy(&search_result.stdout);
        let mut found = false;
        for file in files.lines() {
            let file_path = nixpkgs_path.join(file);
            if update_hash_in_file(&file_path, &mismatch.specified, &mismatch.got)? {
                updated_file = file_path;
                found = true;
                break;
            }
        }

        if !found {
            error!("Could not find and update hash in any file");
            return Ok(false);
        }
    }

    // Step 5: Rebuild with the updated hash
    info!("Step 5: Rebuilding with updated hash...");

    let (rebuild_success, rebuild_logs, new_store_path) = build_intermediate(
        nixpkgs_path,
        attribute,
        intermediate,
        system,
        "after",
    ).await?;

    if !rebuild_success {
        error!("Rebuild failed after hash update. Logs:\n{}", rebuild_logs);
        return Ok(false);
    }

    info!("Rebuild succeeded!");
    let new_store_path = new_store_path.ok_or_else(|| anyhow!("No store path returned from successful build"))?;
    info!("New store path: {}", new_store_path);

    // Copy new source to logs
    let mut has_new_source = false;
    {
        let log_dir = get_fix_hash_attr_log_dir(attribute)?;
        let new_src_dest = log_dir.join(format!("{}.src.after", intermediate));
        match copy_source_to_logs(&new_store_path, &new_src_dest).await {
            Ok(()) => {
                info!("Copied new source to: {}", new_src_dest.display());
                has_new_source = true;
            }
            Err(e) => {
                warn!("Failed to copy new source: {}", e);
            }
        }
    }

    // Step 6: Diff the old and new sources (if not --dont-diff)
    let mut has_diff = false;
    let mut diff_failed_reason: Option<String> = None;

    if !dont_diff {
        info!("Step 6: Running difftastic to diff sources...");

        if let Some(ref old_path) = old_source_path {
            if std::path::Path::new(old_path).exists() {
                let diff_output = run_difftastic(old_path, &new_store_path).await?;

                // Write the diff log
                let final_log_path = if let Some(loc) = log_location {
                    loc.clone()
                } else {
                    let log_dir = get_fix_hash_attr_log_dir(attribute)?;
                    log_dir.join(format!("{}.diff.log", intermediate))
                };

                std::fs::write(&final_log_path, &diff_output)
                    .with_context(|| format!("Failed to write diff log to {}", final_log_path.display()))?;

                info!("Diff log written to: {}", final_log_path.display());
                has_diff = true;

                let line_count = diff_output.lines().count();
                info!("Diff contains {} lines", line_count);
            } else {
                diff_failed_reason = Some("old store path exists in database but not on disk".to_string());
                info!("Old store path doesn't exist on disk, skipping diff");
            }
        } else {
            diff_failed_reason = Some("could not fetch old source from substituters".to_string());
            info!("Could not fetch old source from substituters, skipping diff");
        }
    } else {
        info!("Step 6: Skipping diff (--dont-diff specified)");
        diff_failed_reason = Some("--dont-diff was specified".to_string());
    }

    // Step 7: Commit and push if --origin is specified
    let after_commit = if let Some(origin_remote) = origin {
        info!("Step 7: Committing and pushing changes...");

        // Extract short hash (first 8 chars after "sha256-")
        let short_hash = mismatch.got
            .strip_prefix("sha256-")
            .unwrap_or(&mismatch.got)
            .chars()
            .take(8)
            .collect::<String>();

        let final_branch = branch
            .map(|s| s.clone())
            .unwrap_or_else(|| format!("fix/{}-{}-{}", attribute.replace('.', "-"), intermediate, short_hash));

        let new_commit = git_commit_and_push(
            nixpkgs_path,
            attribute,
            intermediate,
            &mismatch.specified,
            &mismatch.got,
            &updated_file,
            origin_remote,
            &final_branch,
        )
        .await?;

        info!("Changes pushed successfully!");
        new_commit
    } else {
        info!("Step 7: Skipping git push (no --origin specified)");
        // No commit was made, use the before commit as placeholder
        nixpkgs_commit.trim().to_string()
    };

    // Step 8: Generate and print PR text
    if !no_pr_text {
        info!("Step 8: Generating PR text...");

        // Read log files for inclusion in PR text
        let log_dir = get_fix_hash_attr_log_dir(attribute)?;
        let before_log = std::fs::read_to_string(log_dir.join(format!("{}.before.log", intermediate))).ok();
        let after_log = std::fs::read_to_string(log_dir.join(format!("{}.after.log", intermediate))).ok();
        let diff_content = if has_diff {
            std::fs::read_to_string(log_dir.join(format!("{}.diff.log", intermediate))).ok()
        } else {
            None
        };

        let result = FixHashResult {
            attribute: attribute.to_string(),
            intermediate: intermediate.to_string(),
            before_commit: nixpkgs_commit.trim().to_string(),
            after_commit,
            has_diff,
            diff_failed_reason: diff_failed_reason.clone(),
            has_old_source,
            has_new_source,
            before_log,
            after_log,
            diff_content,
        };

        let pr_text = generate_pr_text(&result, log_base_url);

        // Print PR text to stdout
        println!("\n{}", "=".repeat(60));
        println!("PR TEXT:");
        println!("{}", "=".repeat(60));
        println!("{}", pr_text);
        println!("{}", "=".repeat(60));
    }

    info!("==========================================");
    info!("Hash fix completed successfully!");
    info!("Updated: {} -> {}", mismatch.specified, mismatch.got);
    info!("==========================================");

    Ok(true)
}

/// Main function to process a fix-hash operation
pub async fn process_fix_hash(
    nixpkgs_path: &PathBuf,
    attribute: &str,
    intermediate: &str,
    system: &str,
    dont_diff: bool,
    log_location: Option<&PathBuf>,
    nixpkgs_ref: &str,
    origin: Option<&String>,
    branch: Option<&String>,
    log_base_url: &str,
    no_pr_text: bool,
) -> Result<bool> {
    info!("==========================================");
    info!("Fix Hash: {} ({})", attribute, intermediate);
    info!("==========================================");

    info!("Step 1: Creating worktree for ref: {}", nixpkgs_ref);

    // Only fetch if origin/master is older than a week
    let should_fetch = match run_command_async("git", &[
        "-C", nixpkgs_path.to_str().unwrap(),
        "log", "-1", "--format=%ct", "origin/master"
    ])
    .await
    {
        Ok(timestamp_str) => {
            if let Ok(commit_time) = timestamp_str.trim().parse::<i64>() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let one_week_secs = 7 * 24 * 60 * 60;
                let age = now - commit_time;
                if age > one_week_secs {
                    info!("origin/master is {} days old, fetching...", age / (24 * 60 * 60));
                    true
                } else {
                    info!("origin/master is {} days old, skipping fetch", age / (24 * 60 * 60));
                    false
                }
            } else {
                // Couldn't parse timestamp, fetch to be safe
                true
            }
        }
        Err(_) => {
            // Couldn't get commit time (maybe origin/master doesn't exist), fetch
            true
        }
    };

    if should_fetch {
        let _ = run_command_async("git", &[
            "-C", nixpkgs_path.to_str().unwrap(),
            "fetch", "origin", "--no-tags"
        ])
        .await;
    }

    // Create temp dir and worktree with unique prefix including attribute name
    let prefix = format!("fix-hash-{}-{}-", attribute.replace('.', "_"), intermediate);
    let temp_dir = TempDir::with_prefix(&prefix)?;
    let worktree_path = temp_dir.path().join("nixpkgs");

    // Create worktree
    // We use --detach to avoid conflicts if the ref is a branch already checked out elsewhere
    run_command_async("git", &[
        "-C", nixpkgs_path.to_str().unwrap(),
        "worktree", "add", "--detach", "-f",
        worktree_path.to_str().unwrap(),
        nixpkgs_ref
    ])
    .await
    .context("Failed to create worktree")?;

    // Call inner function with the worktree path
    let result = process_fix_hash_worktree(
        &worktree_path,
        attribute,
        intermediate,
        system,
        dont_diff,
        log_location,
        origin,
        branch,
        log_base_url,
        no_pr_text,
    ).await;

    // Check if we should preserve the temp dir on failure
    match &result {
        Ok(true) => {
            // Success - cleanup worktree
            let _ = run_command_async("git", &[
                "-C", nixpkgs_path.to_str().unwrap(),
                "worktree", "remove", "-f",
                worktree_path.to_str().unwrap()
            ])
            .await;
        }
        Ok(false) | Err(_) => {
            // Failure - preserve the temp directory for user to debug
            let preserved_path = temp_dir.keep();
            let worktree_in_preserved = preserved_path.join("nixpkgs");

            error!("Fix-hash failed. Preserving temp nixpkgs checkout for debugging.");
            println!("\n========================================");
            println!("PRESERVED NIXPKGS CHECKOUT: {}", worktree_in_preserved.display());
            println!("========================================");
            println!("To clean up when done:");
            println!("  git -C {} worktree remove -f {}", nixpkgs_path.display(), worktree_in_preserved.display());
            println!("  rm -rf {}", preserved_path.display());
            println!("========================================\n");
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hash_mismatch() {
        let output = r#"
error: hash mismatch in fixed-output derivation '/nix/store/abc123.drv':
  specified: sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
  got:       sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=
"#;

        let mismatch = parse_hash_mismatch(output).unwrap();
        assert_eq!(mismatch.specified, "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        assert_eq!(mismatch.got, "sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=");
    }

    #[test]
    fn test_parse_hash_mismatch_real_example() {
        let output = r#"
building '/nix/store/xyz.drv'...

error: hash mismatch in fixed-output derivation '/nix/store/abc-source.drv':
         specified: sha256-rN5fIBnCEcTT+8UYsSOmPKT8SLXWD/c9nAo0MxrqKPw=
               got: sha256-kGfmBEWEn1a/y8gxEUxUJqWl94FkGq/5wZTdGkz8V8M=
"#;

        let mismatch = parse_hash_mismatch(output).unwrap();
        assert!(mismatch.specified.starts_with("sha256-"));
        assert!(mismatch.got.starts_with("sha256-"));
    }
}
