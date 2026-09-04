//! Shared state: connected Vim instances, the selected instance, and the
//! request/response plumbing over the Unix socket.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;

use crate::protocol::{InstanceInfo, Request, ResponseError};

pub const SOCKET_PATH: &str = "/tmp/vim-mcp-server.sock";
pub const REGISTRY_PATH: &str = "/tmp/vim-mcp-registry.json";
pub const PREFERENCE_PATH: &str = "/tmp/vim-mcp-preference.txt";

/// Reply channel for a single outstanding request: `Ok` on a result, `Err` on
/// a Vim-reported error.
pub type ResponseSender = oneshot::Sender<Result<Value, ResponseError>>;

/// Map of in-flight request ids to their awaiting reply channels.
pub type PendingMap = Arc<Mutex<HashMap<i64, ResponseSender>>>;

/// Outbound channel to a single connected Vim instance and its pending
/// request map. Each connection task owns the socket write half via `tx`.
pub struct Connection {
    pub info: InstanceInfo,
    pub started: String,
    /// Sends a fully-framed (newline-terminated) line to the Vim socket.
    pub tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Requests awaiting a response, keyed by request id.
    pub pending: PendingMap,
    /// Latest cached state pushed via `state_update`.
    pub state: Mutex<Option<Value>>,
}

#[derive(Default)]
pub struct Registry {
    pub connections: Mutex<HashMap<String, Arc<Connection>>>,
    pub selected: Mutex<Option<String>>,
    pub pending_exits: Mutex<std::collections::HashSet<String>>,
    next_id: AtomicI64,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Registry::default())
    }

    fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Persist the current instance list to the registry file. Best-effort.
    pub async fn save_registry(&self) {
        let conns = self.connections.lock().await;
        let mut registry = serde_json::Map::new();
        for (id, conn) in conns.iter() {
            registry.insert(
                id.clone(),
                json!({
                    "pid": conn.info.pid,
                    "cwd": conn.info.cwd,
                    "main_file": conn.info.main_file,
                    "buffers": conn.info.buffers,
                    "started": conn.started,
                }),
            );
        }
        if let Ok(text) = serde_json::to_string_pretty(&Value::Object(registry)) {
            let _ = tokio::fs::write(REGISTRY_PATH, text).await;
        }
    }

    pub async fn save_preference(&self, instance_id: &str) {
        let _ = tokio::fs::write(PREFERENCE_PATH, instance_id).await;
    }

    pub async fn get_connection(&self, id: &str) -> Option<Arc<Connection>> {
        self.connections.lock().await.get(id).cloned()
    }

    /// Send a request to a Vim instance and await its response, with a timeout.
    pub async fn request(
        &self,
        instance_id: &str,
        method: &'static str,
        params: Value,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        let conn = self
            .get_connection(instance_id)
            .await
            .ok_or_else(|| format!("Instance {instance_id} not connected"))?;

        let id = self.next_request_id();
        let (resp_tx, resp_rx) = oneshot::channel();
        conn.pending.lock().await.insert(id, resp_tx);

        let line = serde_json::to_string(&Request { id, method, params })
            .map_err(|e| e.to_string())?
            + "\n";
        conn.tx
            .send(line)
            .map_err(|_| format!("Instance {instance_id} disconnected"))?;

        match timeout(Duration::from_millis(timeout_ms), resp_rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => Err(err.message),
            Ok(Err(_)) => Err("Vim connection closed before responding".to_string()),
            Err(_) => {
                conn.pending.lock().await.remove(&id);
                Err("Timeout waiting for Vim response".to_string())
            }
        }
    }

    /// Convenience: request the full Vim state (`get_state`), 2s timeout,
    /// caching the result on the connection.
    pub async fn request_state(&self, instance_id: &str) -> Result<Value, String> {
        let state = self
            .request(instance_id, "get_state", json!({}), 2000)
            .await?;
        if let Some(conn) = self.get_connection(instance_id).await {
            *conn.state.lock().await = Some(state.clone());
        }
        Ok(state)
    }
}
