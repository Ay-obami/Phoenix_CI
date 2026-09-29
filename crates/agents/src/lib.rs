//! Agent/LLM abstraction layer (Phase 3). Kept OUT of reliability logic
//! (BUILD_PROMPT §0.5): nothing in core/supervisor/worker imports this crate.
//! Agents PROPOSE — plans and fixes; the deterministic gate still owns truth.
//!
//! Provider-agnostic per spec: everything talks to [`llm::LlmProvider`];
//! keys/base URLs come from env (`LLM_PROVIDER`, `ANTHROPIC_API_KEY`,
//! `ANTHROPIC_BASE_URL`, `ANTHROPIC_MODEL`) and are never hardcoded.

pub mod implementer;
pub mod llm;
pub mod plan;
mod process;
pub mod workspace;

pub use implementer::ImplementerAgent;
pub use llm::{
    provider_from_env, AnthropicProvider, FallbackProvider, GoogleProvider, GroqProvider,
    LlmError, LlmProvider, MockLlmProvider,
};
pub use plan::{planner_prompt, FixPlan, PlannedEdit};
pub use workspace::{
    filesystem_repo, parse_github_pr, publish_fix, resolve_push_branch, SandboxSource,
};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),

    #[error("could not parse a valid fix plan from the model output: {0}")]
    PlanParse(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("unsafe path in plan edit: {0}")]
    UnsafePath(String),

    #[error("git operation failed: {0}")]
    Git(String),

}
