use crate::termwindow::TermWindowNotif;
use anyhow::{anyhow, Context};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;
use window::WindowOps;

const UI_SOCKET_ENV: &str = "H2_UI_SOCKET";
const NODE_SOCKET_ENV: &str = "H2_NODE_SOCKET";
const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
static STATUS_BRIDGE: Once = Once::new();

pub fn maybe_start_status_bridge() {
    let socket_paths = status_socket_paths_from_values(
        std::env::var_os(UI_SOCKET_ENV),
        std::env::var_os(NODE_SOCKET_ENV),
    );
    if socket_paths.is_empty() {
        return;
    }

    STATUS_BRIDGE.call_once(move || {
        if let Err(err) = std::thread::Builder::new()
            .name("h2-status-bridge".to_string())
            .spawn(move || run_status_bridge(socket_paths))
        {
            log::warn!("failed to start h2 status bridge: {err:#}");
        }
    });
}

fn status_socket_paths_from_values(
    ui_socket: Option<OsString>,
    node_socket: Option<OsString>,
) -> Vec<PathBuf> {
    IntoIterator::into_iter([ui_socket, node_socket])
        .flatten()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn run_status_bridge(socket_paths: Vec<PathBuf>) {
    loop {
        let status_line = match poll_status_line(&socket_paths) {
            Ok(line) => line,
            Err(err) => {
                log::trace!("h2 status bridge poll failed: {err:#}");
                "H2 disconnected".to_string()
            }
        };
        set_right_status(status_line);
        std::thread::sleep(Duration::from_millis(poll_interval_ms()));
    }
}

fn poll_interval_ms() -> u64 {
    std::env::var("H2_UI_STATUS_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value >= 250)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS)
}

fn poll_status_line(socket_paths: &[PathBuf]) -> anyhow::Result<String> {
    let mut last_error = None;
    for socket_path in socket_paths {
        match send_rpc(
            socket_path,
            "ui_snapshot",
            json!({
                "from_seq": 0,
                "event_limit": 5,
                "artifact_limit": 3
            }),
        )
        .context("ui_snapshot")
        {
            Ok(snapshot) => return Ok(format_snapshot_status_line(&snapshot)),
            Err(err) => {
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no h2 socket candidates")))
}

fn set_right_status(status: String) {
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(front_end) = crate::frontend::try_front_end() {
            for window in front_end.gui_windows() {
                window
                    .window
                    .notify(TermWindowNotif::SetRightStatus(status.clone()));
            }
        }
    })
    .detach();
}

fn format_snapshot_status_line(snapshot: &Value) -> String {
    let seq = snapshot
        .get("status")
        .and_then(|status| status.get("events_seq"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let nodes = snapshot
        .get("status")
        .and_then(|status| status.get("registered_nodes"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let artifact_count = snapshot
        .get("artifact_count")
        .and_then(Value::as_u64)
        .or_else(|| {
            snapshot
                .get("artifacts")
                .and_then(Value::as_array)
                .map(|items| items.len() as u64)
        })
        .unwrap_or_default();
    let last_event = snapshot
        .get("events")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("idle");

    format!("H2 seq={seq} nodes={nodes} artifacts={artifact_count} last={last_event}")
}

fn send_rpc(socket_path: &Path, method: &str, params: Value) -> anyhow::Result<Value> {
    send_rpc_impl(socket_path, method, params)
}

#[cfg(unix)]
fn send_rpc_impl(socket_path: &Path, method: &str, params: Value) -> anyhow::Result<Value> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path)
        .with_context(|| format!("connect {}", socket_path.display()))?;
    let timeout = Some(Duration::from_millis(750));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params
    });
    let payload = serde_json::to_vec(&request)?;
    stream.write_all(&(payload.len() as u32).to_be_bytes())?;
    stream.write_all(&payload)?;

    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 8 * 1024 * 1024 {
        anyhow::bail!("response frame too large: {len}");
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    let response: Value = serde_json::from_slice(&payload)?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("rpc error: {error}");
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("missing rpc result"))
}

#[cfg(not(unix))]
fn send_rpc_impl(_socket_path: &Path, _method: &str, _params: Value) -> anyhow::Result<Value> {
    anyhow::bail!("h2 status bridge requires unix sockets")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_status_line_uses_one_rpc_payload() {
        let snapshot = serde_json::json!({
            "status": {
                "events_seq": 52,
                "registered_nodes": 4
            },
            "artifact_count": 7,
            "artifacts": [
                {"artifact": "b", "kind": "decision", "seq": 51}
            ],
            "events": [
                {"seq": 52, "type": "trace_log"}
            ]
        });

        assert_eq!(
            format_snapshot_status_line(&snapshot),
            "H2 seq=52 nodes=4 artifacts=7 last=trace_log"
        );
    }

    #[test]
    fn status_bridge_orders_ui_socket_before_node_socket() {
        let chosen = status_socket_paths_from_values(
            Some("temp/h2-runtime/ui.sock".into()),
            Some("temp/h2-runtime/node.sock".into()),
        );

        assert_eq!(
            chosen,
            vec![
                PathBuf::from("temp/h2-runtime/ui.sock"),
                PathBuf::from("temp/h2-runtime/node.sock")
            ]
        );
    }

    #[test]
    fn status_bridge_falls_back_to_node_socket() {
        let chosen =
            status_socket_paths_from_values(None, Some("temp/h2-runtime/node.sock".into()));

        assert_eq!(chosen, vec![PathBuf::from("temp/h2-runtime/node.sock")]);
    }
}
