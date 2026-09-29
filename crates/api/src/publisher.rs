//! GitHub publisher: listens for `merge.opened` and merges the real pull
//! request on GitHub via the `gh` CLI. Best-effort by design — a GitHub
//! action failing must NEVER affect the deterministic gate decision; it only
//! emits a visible `github.pr_merged` / `github.action_failed` event.
//!
//! Precondition honoured by the implementer: the fix is ALREADY pushed to the
//! PR's head branch (and proven by a real test run) before the gate opens, so
//! merging here merges the verified code.

use std::process::Stdio;

use swarm_agents::workspace::{child_command, filesystem_repo, parse_github_pr, resolve_push_branch};
use swarm_core::events::SwarmEvent;
use swarm_core::task::TaskSpec;
use swarm_supervisor::Supervisor;

use crate::state::TaskRegistry;

/// Consume the event stream; merge GitHub PRs as their gates open (detached
/// task, never participates in governance).
pub async fn run(supervisor: Supervisor, tasks: TaskRegistry) {
    let events = supervisor.events().clone();
    let mut rx = supervisor.subscribe();
    tracing::info!("github publisher listening");
    while let Ok(ev) = rx.recv().await {
        let SwarmEvent::MergeOpened { task_id } = ev else { continue };
        let Some(spec) = tasks.lock().unwrap().get(&task_id).cloned() else {
            continue;
        };
        match merge_pr(&spec) {
            Ok(Some(url)) => events.emit(SwarmEvent::GithubPrMerged { task_id, url }),
            Ok(None) => {
                tracing::info!(task_id = %task_id, "no open GitHub PR to merge for this task (branch form, or already merged)")
            }
            Err(reason) => {
                tracing::warn!(task_id=%task_id, %reason, "github merge action failed");
                events.emit(SwarmEvent::GithubActionFailed { task_id, reason });
            }
        }
    }
}

/// Merge the PR behind `spec`. Returns `Some(pr_url)` when a real PR was
/// merged, `None` when there is nothing to merge (no GitHub target, or no
/// open PR for the branch), and `Err` on an actionable failure.
fn merge_pr(spec: &TaskSpec) -> Result<Option<String>, String> {
    // Resolve (repo, pr_number, pr_url).
    let (repo, number, pr_url) = if let Some(url) = spec.pr_url.as_deref() {
        let (repo, number) = parse_github_pr(url)
            .ok_or_else(|| format!("cannot parse pr_url: {url}"))?;
        (repo, Some(number as u64), Some(url.to_string()))
    } else if let (Some(repo_url), Some(r#ref)) =
        (spec.repo_url.as_deref(), spec.git_ref.as_deref())
    {
        let repo = filesystem_repo(repo_url);
        match find_open_pr_number(&repo, r#ref) {
            Some(number) => {
                let pr_url = format!("https://github.com/{repo}/pull/{number}");
                (repo, Some(number), Some(pr_url))
            }
            None => return Ok(None),
        }
    } else {
        return Ok(None); // local fixture dev mode
    };

    let Some(number) = number else { return Ok(None) };
    let merged = gh_ok(&["pr", "merge", &number.to_string(), "--repo", &repo, "--squash"]);
    if merged {
        Ok(pr_url.map(|p| p).or_else(|| Some(format!("https://github.com/{repo}/pull/{number}"))))
    } else {
        Err(format!("`gh pr merge {number}` failed (no auth / not mergeable / already merged)"))
    }
}

fn find_open_pr_number(repo: &str, r#ref: &str) -> Option<u64> {
    let branch = resolve_push_branch(&format!("https://github.com/{repo}.git"), r#ref)?;
    let out = child_command("gh")
        .args(["pr", "list", "--state", "open", "--head", &branch, "--repo", repo, "--json", "number"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let nums: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    nums.first()?.get("number")?.as_u64()
}

fn gh_ok(args: &[&str]) -> bool {
    let mut cmd = child_command("gh");
    cmd.stdin(Stdio::null());
    for a in args {
        cmd.arg(a);
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}
