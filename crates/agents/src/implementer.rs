//! The implementer agent: sandbox prep → plan → edits → REAL `cargo test`.
//!
//! Provenance honesty lives HERE: pass/fail comes from the process exit
//! status of a genuine cargo run inside the sandbox — never from any model
//! claim. This is what the deterministic merge gate keys off.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use swarm_core::events::TestOrigin;
use swarm_core::gate::TestReport;
use swarm_core::ids::TaskId;
use swarm_core::task::TaskSpec;
use tracing::Instrument;

use crate::llm::LlmProvider;
use crate::plan::{planner_prompt, FixPlan};
use crate::workspace;
use crate::AgentError;

pub struct ImplementerAgent {
    provider: Arc<dyn LlmProvider>,
    /// Legacy/dev fallback source copied when the task has no git origin.
    fixture_src: PathBuf,
    /// Parent directory for per-attempt sandbox copies.
    sandbox_root: PathBuf,
}

impl ImplementerAgent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        fixture_src: PathBuf,
        sandbox_root: PathBuf,
    ) -> Self {
        Self {
            provider,
            fixture_src,
            sandbox_root,
        }
    }

    /// One attempt's full flow for one task spec. FS/subprocess calls are
    /// synchronous here — short, and workers own their task.
    pub async fn fix(&self, spec: &TaskSpec) -> Result<TestReport, AgentError> {
        // Demo knob: widen the mid-flight window for live KILLs.
        static PLAN_DELAY: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
        let delay = *PLAN_DELAY.get_or_init(|| {
            std::time::Duration::from_millis(
                std::env::var("SWARM_PLAN_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            )
        });
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        // 1. Materialize the working copy OFF the async thread — a slow git
        // clone here must not starve this worker's heartbeat timer.
        let sandbox = self.sandbox_root.join(TaskId::generate().to_string());
        // Remember the git origin so a passing fix can be pushed back later.
        let git_origin: Option<(String, String)> = match workspace::resolve(spec) {
            Some(workspace::SandboxSource::Git { url, fetch_ref }) => Some((url, fetch_ref)),
            _ => None,
        };
        {
            let source = workspace::resolve(spec);
            let fixture = self.fixture_src.clone();
            let dest = sandbox.clone();
            tokio::task::spawn_blocking(move || {
                workspace::prepare(source.as_ref(), &fixture, &dest)
            })
            .await
            .map_err(|e| AgentError::Git(format!("prepare join error: {e}")))??;
        }

        // 2. Read the ACTUAL code so the planner fixes reality.
        let context = collect_context(&sandbox)?;


        // 3. Planner → typed plan. One free re-ask on unparseable output so
        // transient model sloppiness doesn't burn a whole supervisor attempt.
        let mut prompt = planner_prompt(&spec.bug_description, &context);
        let plan = {
            let mut plan = None;
            for try_n in 1..=2 {
                if try_n == 2 {
                    prompt.push_str(
                        "\n\nREMINDER: your previous reply was not usable. Respond AGAIN \
                         with ONLY one valid JSON object matching the schema — no prose, \
                         no markdown fences, no text after the object.",
                    );
                }
                let raw = self
                    .provider
                    .complete(&prompt)
                    .instrument(tracing::info_span!("planner", attempt_of_try = try_n))
                    .await?;
                match FixPlan::from_llm_text(&raw) {
                    Ok(p) => {
                        plan = Some(p);
                        break;
                    }
                    Err(e) if try_n == 1 => {
                        tracing::warn!(error = %e, "plan parse failed; retrying once");
                    }
                    Err(e) => return Err(e),
                }
            }
            plan.expect("loop returns on second failure")
        };


        // 4. Apply whole-file replacements.
        {
            let _guard = tracing::info_span!("apply_edits").entered();
            for edit in &plan.edits {
                // Paths were validated at parse time (no absolute / no `..`).
                let dest = sandbox.join(&edit.path);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, &edit.content)?;
            }
        }

        // 5. THE ONLY source of truth for pass/fail: real cargo test, timed,
        //    and run OFF the async thread (blocking process wait must not
        //    starve the heartbeat timer either).
        let output = {
            let sandbox = sandbox.clone();
            tokio::task::spawn_blocking(move || {
                let _span = tracing::info_span!("cargo_test").entered();
                let started_at = std::time::Instant::now();

                let out = workspace::child_command("cargo")
                    .args(["test", "--quiet"])
                    .current_dir(&sandbox)
                    .output()
                    .map_err(AgentError::from)?;
                tracing::info!(
                    duration_ms = started_at.elapsed().as_millis() as u64,
                    passed = out.status.success(),
                    "cargo test finished"
                );
                Ok::<_, AgentError>(out)
            })
            .await
            .map_err(|e| AgentError::Git(format!("cargo join error: {e}")))??
        };


        let passed = output.status.success();
        // Push the PROVEN fix back before reporting, so merge.opened always
        // means the PR's head branch already carries the verified code.
        if passed {
            if let Some((url, fetch_ref)) = &git_origin {
                if let Some(branch) = workspace::resolve_push_branch(url, fetch_ref) {
                    let msg = format!(
                        "Phoenix CI: {}\n\nApplied in an isolated sandbox and verified by a real `cargo test` run.",
                        spec.title
                    );
                    let dest = sandbox.clone();
                    let url2 = url.clone();
                    let branch2 = branch.clone();
                    let m2 = msg;
                    tokio::task::spawn_blocking(move || {
                        workspace::publish_fix(&dest, &url2, &branch2, &m2)
                    })
                    .await
                    .map_err(|e| AgentError::Git(format!("publish join error: {e}")))??;
                    tracing::info!(branch = %branch, "proven fix pushed to PR head branch");
                } else {
                    tracing::warn!("no head branch resolved; verified fix stays local to the sandbox");
                }
            }
        }
        let summary = summarize(&output);

        // Keep passing sandboxes around for demo inspection; clean up failures.
        if !passed {
            let _ = std::fs::remove_dir_all(&sandbox);
        }

        Ok(TestReport {
            passed,
            origin: TestOrigin::RealCargoTest,
            summary,
        })
    }
}

/// Recursively list text files (relative paths) under `root` for planner
/// context, skipping build-artifact dirs.
fn collect_context(root: &Path) -> std::io::Result<Vec<(String, String)>> {
    fn walk(
        dir: &Path,
        base: &Path,
        out: &mut Vec<(String, String)>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if name == "target" || name == ".git" {
                    continue;
                }
                walk(&path, base, out)?;
            } else if let Ok(content) = std::fs::read_to_string(&path) {
                let rel = match path.strip_prefix(base) {
                    Ok(rel) => rel.to_string_lossy().into_owned(),
                    Err(_) => continue,
                };
                out.push((rel, content));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn summarize(output: &std::process::Output) -> String {
    let combined = format!(
        "exit={:?}\nstderr:\n{}\nstdout(tail):\n{}",
        output.status.code(),
        tail(String::from_utf8_lossy(&output.stderr).as_ref(), 600),
        tail(String::from_utf8_lossy(&output.stdout).as_ref(), 400)
    );
    tail(&combined, 1000)
}

/// Keep the TAIL (cargo prints its failure summary last).
fn tail(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().skip(count - max_chars).collect()
}
