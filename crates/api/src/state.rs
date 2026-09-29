//! Shared handler state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use swarm_core::ids::TaskId;
use swarm_core::task::TaskSpec;
use swarm_supervisor::Supervisor;

use crate::spawner::DemoSpawner;

/// Submitted tasks: id → the exact spec they were submitted with. The
/// publisher reads this to find the `pr_url` behind a `merge.opened`.
pub type TaskRegistry = Arc<Mutex<HashMap<TaskId, TaskSpec>>>;

#[derive(Clone)]
pub struct AppState {
    pub supervisor: Supervisor,
    pub tasks: TaskRegistry,
    /// Demo fault-injection registry (join handles per worker generation).
    pub spawner: Arc<DemoSpawner>,
    /// None only for the simulated local demo. Real-agent writes require this token.
    pub submit_token: Option<Arc<str>>,
}
