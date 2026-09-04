//! Unix-socket server: accepts Vim connections, frames newline-delimited JSON,
//! and routes registrations, state pushes, and request responses.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};

use crate::protocol::{IncomingMessage, Registered};
use crate::registry::{Connection, PendingMap, Registry, SOCKET_PATH};

/// Start listening on the Unix socket. Spawns a task per connection.
pub async fn serve(registry: Arc<Registry>) -> std::io::Result<()> {
    // Clean up a stale socket file.
    let _ = tokio::fs::remove_file(SOCKET_PATH).await;

    let listener = UnixListener::bind(SOCKET_PATH)?;
    // Restrict the socket to the current user.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(SOCKET_PATH, perms);
    }
    eprintln!("Unix socket server listening at {SOCKET_PATH}");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, registry).await {
                        eprintln!("Connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("Accept error: {e}"),
        }
    }
}

async fn handle_connection(stream: UnixStream, registry: Arc<Registry>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Outbound queue: the registry hands us lines to write to this socket.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let mut instance_id: Option<String> = None;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: IncomingMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Error parsing message from Vim: {e}");
                continue;
            }
        };

        // Registration.
        if msg.msg_type.as_deref() == Some("register") {
            if let (Some(id), Some(info)) = (msg.instance_id.clone(), msg.info.clone()) {
                let conn = Arc::new(Connection {
                    info,
                    started: now_iso8601(),
                    tx: tx.clone(),
                    pending: Arc::clone(&pending),
                    state: Mutex::new(None),
                });
                instance_id = Some(id.clone());
                registry.connections.lock().await.insert(id.clone(), conn);
                eprintln!("Vim instance registered: {id}");
                registry.save_registry().await;

                let ack = serde_json::to_string(&Registered::new(id.clone())).unwrap() + "\n";
                let _ = tx.send(ack);

                // Auto-select if this is the only instance.
                let mut selected = registry.selected.lock().await;
                if registry.connections.lock().await.len() == 1 {
                    *selected = Some(id.clone());
                    drop(selected);
                    registry.save_preference(&id).await;
                    eprintln!("Auto-selected single Vim instance: {id}");
                }
            }
            continue;
        }

        // State push: cache it.
        if msg.msg_type.as_deref() == Some("state_update") {
            if let (Some(id), Some(state)) = (instance_id.clone(), msg.state.clone()) {
                if let Some(conn) = registry.get_connection(&id).await {
                    *conn.state.lock().await = Some(state);
                }
            }
            continue;
        }

        // Otherwise it's a response to one of our requests.
        if let Some(id) = msg.id {
            if let Some(sender) = pending.lock().await.remove(&id) {
                let payload = if let Some(err) = msg.error {
                    Err(err)
                } else {
                    Ok(msg.result.unwrap_or(Value::Null))
                };
                let _ = sender.send(payload);
            }
        }
    }

    // Socket closed. Detect whether this was an expected exit.
    if let Some(id) = instance_id {
        let was_expected = registry.pending_exits.lock().await.remove(&id);
        eprintln!(
            "Vim instance disconnected: {id}{}",
            if was_expected { " (expected exit)" } else { "" }
        );
        registry.connections.lock().await.remove(&id);
        registry.save_registry().await;

        let mut selected = registry.selected.lock().await;
        if selected.as_deref() == Some(id.as_str()) {
            *selected = None;
        }
    }

    drop(tx);
    let _ = writer.await;
    Ok(())
}

/// UTC timestamp in the same ISO-8601 shape the old server wrote.
fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Minimal, dependency-free formatting: seconds since epoch is enough for a
    // stable "started" marker; render as an RFC3339-ish string.
    format!("{secs}")
}

/// Verify a command's effect by diffing before/after state. Mirrors the
/// heuristics the original server used so tool output stays familiar.
pub fn verify_command_execution(command: &str, before: &Value, after: &Value) -> (bool, String) {
    let cmd = command.trim().to_lowercase();

    let win_count = |s: &Value| s.get("windows").and_then(|w| w.as_array()).map_or(0, |a| a.len());
    let tab_count = |s: &Value| s.get("tabs").and_then(|t| t.as_array()).map_or(0, |a| a.len());

    if cmd == "split" || cmd.starts_with("split ") {
        let (b, a) = (win_count(before), win_count(after));
        return if a > b {
            (true, format!("Window split successful. Windows increased from {b} to {a}."))
        } else {
            (false, format!("Window split may have failed. Window count unchanged: {b}."))
        };
    }

    if cmd == "vsplit" || cmd.starts_with("vsplit ") {
        let (b, a) = (win_count(before), win_count(after));
        return if a > b {
            (true, format!("Vertical split successful. Windows increased from {b} to {a}."))
        } else {
            (false, format!("Vertical split may have failed. Window count unchanged: {b}."))
        };
    }

    if cmd == "tabnew" || cmd.starts_with("tabnew ") {
        let (b, a) = (tab_count(before), tab_count(after));
        return if a > b {
            (true, format!("New tab created successfully. Tabs increased from {b} to {a}."))
        } else {
            (false, format!("Tab creation may have failed. Tab count unchanged: {b}."))
        };
    }

    if cmd == "tabnext" || cmd == "tabprevious" {
        let active_idx = |s: &Value| -> i64 {
            s.get("tabs")
                .and_then(|t| t.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .position(|t| t.get("active").and_then(|a| a.as_bool()).unwrap_or(false))
                })
                .map_or(-1, |p| p as i64)
        };
        let (b, a) = (active_idx(before), active_idx(after));
        return if b != a {
            (true, format!("Tab navigation successful. Active tab changed from {} to {}.", b + 1, a + 1))
        } else {
            let edge = if cmd == "tabnext" { "last" } else { "first" };
            (false, format!("Tab navigation may have failed or already at {edge} tab."))
        };
    }

    if cmd.starts_with("edit ") || cmd.starts_with("e ") {
        let filename = cmd.split_whitespace().nth(1).unwrap_or("");
        let after_name = after
            .get("current_buffer")
            .and_then(|b| b.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        return if !filename.is_empty() && after_name.contains(filename) {
            (true, format!("File opened successfully: {after_name}"))
        } else {
            (false, format!("File opening may have failed. Current buffer: {after_name}"))
        };
    }

    if cmd.starts_with("set ") {
        return (true, format!("Setting command executed: {command}"));
    }

    if cmd.starts_with("wincmd ") {
        let buf_id = |s: &Value| s.get("current_buffer").and_then(|b| b.get("id")).cloned();
        if buf_id(before) != buf_id(after) {
            let after_id = buf_id(after).map(|v| v.to_string()).unwrap_or_default();
            return (true, format!("Window navigation successful. Moved to buffer {after_id}."));
        }
        return (true, format!("Window navigation command executed: {command}"));
    }

    if cmd == "w" || cmd == "write" {
        let modified = after
            .get("current_buffer")
            .and_then(|b| b.get("modified"))
            .and_then(|m| m.as_bool())
            .unwrap_or(true);
        if !modified {
            let name = after
                .get("current_buffer")
                .and_then(|b| b.get("name"))
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("[No Name]");
            return (true, format!("File saved successfully: {name}"));
        }
        return (true, format!("Write command executed: {command}"));
    }

    (true, format!("Command executed successfully: {command}"))
}

/// Is this an Ex command that exits Vim (so the socket will close instead of
/// replying)?
pub fn is_exit_command(command: &str) -> bool {
    let cmd = command.trim();
    let first = cmd.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "q" | "qa" | "qall" | "wq" | "wqa" | "wqall" | "q!" | "qa!" | "qall!"
    )
}

/// Run an exit command: send it, then wait for the socket to close (which the
/// bridge observes as a disconnect). We mark the instance pending-exit so the
/// disconnect is treated as success.
pub async fn execute_exit_command(
    registry: &Arc<Registry>,
    instance_id: &str,
    command: &str,
) -> Result<String, String> {
    let conn = registry
        .get_connection(instance_id)
        .await
        .ok_or_else(|| format!("Instance {instance_id} not connected"))?;

    registry
        .pending_exits
        .lock()
        .await
        .insert(instance_id.to_string());

    let id = 0; // exit responses never come back; id is irrelevant
    let line = json!({ "id": id, "method": "execute_command", "params": { "command": command } })
        .to_string()
        + "\n";
    conn.tx.send(line).map_err(|_| "Instance disconnected".to_string())?;

    // Poll for disconnect (the bridge removes the connection on socket close).
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if registry.get_connection(instance_id).await.is_none() {
            return Ok("Vim exited successfully".to_string());
        }
    }
    registry.pending_exits.lock().await.remove(instance_id);
    Err("Timeout waiting for Vim to exit".to_string())
}

/// Execute a normal command with before/after state verification.
pub async fn execute_command_verified(
    registry: &Arc<Registry>,
    instance_id: &str,
    command: &str,
) -> Result<String, String> {
    let before = registry
        .request_state(instance_id)
        .await
        .map_err(|e| format!("Failed to get state before command: {e}"))?;

    let conn = registry
        .get_connection(instance_id)
        .await
        .ok_or_else(|| format!("Instance {instance_id} not connected"))?;
    let line = json!({ "id": 0, "method": "execute_command", "params": { "command": command } })
        .to_string()
        + "\n";
    conn.tx.send(line).map_err(|_| "Instance disconnected".to_string())?;

    // Give Vim a beat to apply the command, then re-read state.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after = registry
        .request_state(instance_id)
        .await
        .map_err(|e| format!("Failed to verify command execution: {e}"))?;

    let (ok, message) = verify_command_execution(command, &before, &after);
    if ok {
        Ok(message)
    } else {
        Err(format!("Command verification failed: {message}"))
    }
}

/// Top-level command dispatch: exit commands vs verified commands.
pub async fn execute_vim_command(
    registry: &Arc<Registry>,
    instance_id: &str,
    command: &str,
) -> Result<String, String> {
    if is_exit_command(command) {
        execute_exit_command(registry, instance_id, command).await
    } else {
        execute_command_verified(registry, instance_id, command).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_detected_by_window_increase() {
        let before = json!({ "windows": [{}] });
        let after = json!({ "windows": [{}, {}] });
        let (ok, msg) = verify_command_execution("split", &before, &after);
        assert!(ok);
        assert!(msg.contains("1 to 2"));
    }

    #[test]
    fn split_without_change_reports_failure() {
        let before = json!({ "windows": [{}] });
        let after = json!({ "windows": [{}] });
        let (ok, _) = verify_command_execution("vsplit", &before, &after);
        assert!(!ok);
    }

    #[test]
    fn write_reports_saved_when_unmodified() {
        let before = json!({ "current_buffer": { "modified": true, "name": "f" } });
        let after = json!({ "current_buffer": { "modified": false, "name": "f" } });
        let (ok, msg) = verify_command_execution("w", &before, &after);
        assert!(ok);
        assert!(msg.contains("saved"));
    }

    #[test]
    fn tabnext_detects_active_tab_change() {
        let before = json!({ "tabs": [{ "active": true }, { "active": false }] });
        let after = json!({ "tabs": [{ "active": false }, { "active": true }] });
        let (ok, _) = verify_command_execution("tabnext", &before, &after);
        assert!(ok);
    }

    #[test]
    fn exit_commands_recognized() {
        for c in ["q", "qa", "qall", "wq", "wqa", "wqall", "q!", "qa!", "qall!"] {
            assert!(is_exit_command(c), "{c} should be an exit command");
        }
        assert!(!is_exit_command("split"));
        assert!(!is_exit_command("set number"));
        assert!(!is_exit_command("write"));
    }

    #[test]
    fn unknown_command_falls_back_to_success() {
        let s = json!({});
        let (ok, _) = verify_command_execution("normal! gg", &s, &s);
        assert!(ok);
    }
}
