//! Git-backed sandbox preparation: turn a `pr_url` / `repo_url` + ref into a
//! working copy the implementer can edit, using the SAME `refs/pull/N/head`
//! convention GitHub uses for real pull requests.

use std::path::{Path, PathBuf};
use std::process::Command;

use swarm_core::task::TaskSpec;

use crate::AgentError;

/// Keep the control-plane write token out of every subprocess, including
/// Cargo tests and git hooks in submitted repositories. This is not sandboxing.
pub fn child_command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_remove("SWARM_API_TOKEN");
    command
}

/// Where a task's working copy comes from.
#[derive(Debug, Clone)]
pub enum SandboxSource {

    /// Legacy/dev mode: copy the local fixture directory.
    Fixture(PathBuf),
    /// Clone from a git remote at an explicit ref.
    Git { url: String, fetch_ref: String },
}

/// Resolve the source for a task. Priority: repo_url > pr_url > fixture.
pub fn resolve(spec: &TaskSpec) -> Option<SandboxSource> {
    if let Some(url) = spec.repo_url.as_deref().filter(|u| !u.trim().is_empty()) {
        let fetch_ref = spec.git_ref.clone().unwrap_or_else(|| "HEAD".into());
        return Some(SandboxSource::Git {
            url: url.to_string(),
            fetch_ref,
        });
    }
    if let Some(pr) = spec.pr_url.as_deref().filter(|u| !u.trim().is_empty()) {
        if let Some((repo, number)) = parse_github_pr(pr) {
            return Some(SandboxSource::Git {
                url: repo,
                fetch_ref: format!("refs/pull/{number}/head"),
            });
        }
    }
    None
}

/// `https://github.com/<owner>/<repo>/pull/42[/*]` →
/// `https://github.com/<owner>/<repo>.git` + 42.
pub fn parse_github_pr(url: &str) -> Option<(String, u32)> {
    let trimmed = url.trim().trim_end_matches('/');
    let idx = trimmed.find("/pull/")?;
    let repo = &trimmed[..idx];
    let num_part = &trimmed[idx + "/pull/".len()..];
    let digits: String = num_part.chars().take_while(char::is_ascii_digit).collect();
    let number = digits.parse().ok()?;
    if repo.is_empty() || number == 0 {
        return None;
    }
    let repo = if repo.ends_with(".git") {
        repo.to_string()
    } else {
        format!("{repo}.git")
    };
    Some((repo, number))
}

/// Map a clone origin (url + fetch_ref) back to the branch name the fix must
/// be pushed to. Pull-request fetch refs (`refs/pull/N/head`) are resolved to
/// their head branch via `gh`; plain branch/tag refs resolve by name.
pub fn resolve_push_branch(url: &str, fetch_ref: &str) -> Option<String> {
    if let Some(n) = fetch_ref
        .strip_prefix("refs/pull/")
        .and_then(|s| s.strip_suffix("/head"))
    {
        let repo = filesystem_repo(url);
        let out = child_command("gh")
            .args(["pr", "view", n, "--repo", &repo, "--json", "headRefName"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        return v["headRefName"].as_str().map(String::from);
    }
    if let Some(b) = fetch_ref.strip_prefix("refs/heads/") {
        return Some(b.to_string());
    }
    if !fetch_ref.starts_with("refs/") {
        return Some(fetch_ref.to_string());
    }
    None
}

pub fn filesystem_repo(url: &str) -> String {
    url.trim_end_matches(".git")
        .trim_start_matches("https://github.com/")
        .trim_start_matches("git@github.com:")
        .replace(':', "/")
}

/// Commit any fix present in the sandbox and push it to the PR's head branch.
/// The caller has ALREADY proven it with a real test run. Uses a bot identity
/// and never prompts on the terminal (`GIT_TERMINAL_PROMPT=0`).
pub fn publish_fix(
    dest: &Path,
    url: &str,
    branch: &str,
    commit_msg: &str,
) -> Result<(), AgentError> {
    // Nothing to do if the fix didn't change anything.
    let status = child_command("git")
        .args(["status", "--porcelain"])
        .current_dir(dest)
        .output()?;
    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Ok(());
    }
    run(dest, &["add", "-A"])?;
    run(dest, &[
        "-c", "user.name=Phoenix CI",
        "-c", "user.email=phoenix-ci@localhost",
        "commit",
        "-q",
        "-m", commit_msg,
    ])?;
    let refspec = format!("HEAD:refs/heads/{branch}");
    let push = child_command("git")
        .args(["push", url, &refspec])
        .current_dir(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    if push.status.success() {
        Ok(())
    } else {
        Err(AgentError::Git(format!(
            "push to {branch} failed: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        )))
    }
}

/// Materialize the source into a fresh directory at `dest`.
pub fn prepare(source: Option<&SandboxSource>, fixture: &Path, dest: &Path) -> Result<(), AgentError> {
    match source {
        None => copy_dir(fixture, dest).map_err(AgentError::from),
        Some(SandboxSource::Fixture(path)) => copy_dir(path, dest).map_err(AgentError::from),
        Some(SandboxSource::Git { url, fetch_ref }) => git_checkout(url, fetch_ref, dest),
    }
}

fn git_checkout(url: &str, fetch_ref: &str, dest: &Path) -> Result<(), AgentError> {
    std::fs::create_dir_all(dest)?;
    run(dest, &[ "init", "-q" ])?;
    run(dest, &["remote", "add", "origin", url])?;
    // Shallow fetch of exactly the ref we care about (works with GitHub's
    // refs/pull/N/head as well as plain branches/tags).
    run(dest, &["fetch", "--depth", "1", "origin", fetch_ref])?;
    run(dest, &["checkout", "--force", "FETCH_HEAD"])?;
    Ok(())
}

fn run(cwd: &Path, args: &[&str]) -> Result<(), AgentError> {
    let out = child_command("git").args(args).current_dir(cwd).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(AgentError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_processes_do_not_inherit_the_api_write_token() {
        let command = child_command("cargo");
        assert!(command.get_envs().any(|(key, value)|
            key == std::ffi::OsStr::new("SWARM_API_TOKEN") && value.is_none()
        ));
    }

    #[test]
    fn parses_github_pr_urls() {
        let (repo, n) =
            parse_github_pr("https://github.com/ayobami/swarm-demo-target/pull/102").unwrap();
        assert_eq!(repo, "https://github.com/ayobami/swarm-demo-target.git");
        assert_eq!(n, 102);
    }

    #[test]
    fn parses_pr_urls_with_suffixes_and_git_ext() {
        let (repo, n) =
            parse_github_pr("https://github.com/o/r/pull/7/files").unwrap();
        assert_eq!(repo, "https://github.com/o/r.git");
        assert_eq!(n, 7);
        let (repo2, n2) = parse_github_pr("https://github.com/o/r.git/pull/9").unwrap();
        assert_eq!(repo2, "https://github.com/o/r.git");
        assert_eq!(n2, 9);
    }

    #[test]
    fn rejects_non_pr_links() {
        assert!(parse_github_pr("https://github.com/o/r/issues/3").is_none());
        assert!(parse_github_pr("not a url").is_none());
    }
}
