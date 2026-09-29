//! Test subprocess lifetime follows the worker future.
use std::process::{Command, Output};
use std::time::Duration;

pub(crate) async fn output(mut command: Command, _limit: Duration) -> std::io::Result<Output> {
    tokio::task::spawn_blocking(move || command.output())
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> (Command, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("phoenix-process-{}", swarm_core::ids::TaskId::generate()));
        std::fs::create_dir_all(&root).unwrap();
        let ready = root.join("ready");
        let marker = root.join("descendant-finished");
        let mut command = crate::workspace::child_command("sh");
        command.args(["-c", "(sleep 1; touch \"$2\") & printf ready > \"$1\"; wait", "test"])
            .arg(&ready).arg(&marker);
        (command, ready, marker)
    }

    async fn ready(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("shell started");
    }

    #[tokio::test]
    async fn cancelling_worker_stops_test_descendants() {
        let (command, started, marker) = fixture();
        let worker = tokio::spawn(output(command, Duration::from_secs(10)));
        ready(&started).await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists(), "descendant executed after worker cancellation");
        std::fs::remove_dir_all(started.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn deadline_stops_test_descendants() {
        let (command, started, marker) = fixture();
        let result = output(command, Duration::from_millis(100)).await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!marker.exists(), "descendant executed after test deadline");
        std::fs::remove_dir_all(started.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn completed_test_preserves_status_and_output() {
        let mut command = crate::workspace::child_command("sh");
        command.args(["-c", "printf output; printf error >&2; exit 7"]);
        let result = output(command, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.status.code(), Some(7));
        assert_eq!(result.stdout, b"output");
        assert_eq!(result.stderr, b"error");
    }
}
