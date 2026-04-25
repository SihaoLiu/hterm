use crate::termwindow::TermWindowNotif;
use anyhow::{anyhow, Context};
use mux::h2::{H2Pane, H2PaneKind};
use mux::pane::Pane;
use mux::tab::{SplitDirection, SplitRequest, SplitSize};
use mux::{Mux, MuxNotification};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;
use window::WindowOps;

const UI_SOCKET_ENV: &str = "H2_UI_SOCKET";
const NODE_SOCKET_ENV: &str = "H2_NODE_SOCKET";
const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
static STATUS_BRIDGE: Once = Once::new();
static DASHBOARD_PANES: Mutex<Option<DashboardPanes>> = Mutex::new(None);

#[derive(Clone)]
struct DashboardPanes {
    graph: Arc<H2Pane>,
    kanban: Arc<H2Pane>,
}

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
        let status_line = match poll_snapshot(&socket_paths) {
            Ok(snapshot) => {
                update_h2_dashboard(snapshot.clone());
                format_snapshot_status_line(&snapshot)
            }
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

fn poll_snapshot(socket_paths: &[PathBuf]) -> anyhow::Result<Value> {
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
            Ok(snapshot) => return Ok(snapshot),
            Err(err) => {
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("no h2 socket candidates")))
}

fn update_h2_dashboard(snapshot: Value) {
    promise::spawn::spawn_into_main_thread(async move {
        let Some(panes) = ensure_h2_dashboard_panes() else {
            return;
        };

        panes.graph.set_lines(graph_lines_from_snapshot(&snapshot));
        panes
            .kanban
            .set_lines(kanban_lines_from_snapshot(&snapshot));

        let mux = Mux::get();
        mux.notify(MuxNotification::PaneOutput(panes.graph.pane_id()));
        mux.notify(MuxNotification::PaneOutput(panes.kanban.pane_id()));
    })
    .detach();
}

fn ensure_h2_dashboard_panes() -> Option<DashboardPanes> {
    if let Some(existing) = DASHBOARD_PANES.lock().ok()?.as_ref().cloned() {
        return Some(existing);
    }
    if std::env::var_os("H2_DISABLE_BOOTSTRAP_PANES").is_some() {
        return None;
    }

    let front_end = crate::frontend::try_front_end()?;
    let gui_window = front_end.gui_windows().into_iter().next()?;
    let mux = Mux::get();
    let tab = mux.get_active_tab_for_window(gui_window.mux_window_id)?;
    let active = tab.get_active_pane()?;
    let active_index = tab
        .iter_panes_ignoring_zoom()
        .iter()
        .find(|pos| pos.pane.pane_id() == active.pane_id())
        .map(|pos| pos.index)?;
    let size = tab.get_size();

    let graph = H2Pane::new(H2PaneKind::Graph, size);
    let kanban = H2Pane::new(H2PaneKind::Kanban, size);

    let graph_pane: Arc<dyn Pane> = graph.clone();
    let kanban_pane: Arc<dyn Pane> = kanban.clone();

    let graph_index = match tab.split_and_insert(
        active_index,
        SplitRequest {
            direction: SplitDirection::Horizontal,
            target_is_second: true,
            top_level: false,
            size: SplitSize::Percent(34),
        },
        graph_pane.clone(),
    ) {
        Ok(index) => index,
        Err(err) => {
            log::warn!("failed to insert H2 graph pane: {err:#}");
            return None;
        }
    };

    if let Err(err) = tab.split_and_insert(
        graph_index,
        SplitRequest {
            direction: SplitDirection::Vertical,
            target_is_second: true,
            top_level: false,
            size: SplitSize::Percent(50),
        },
        kanban_pane.clone(),
    ) {
        log::warn!("failed to insert H2 kanban pane: {err:#}");
        return None;
    }

    if let Err(err) = mux.add_pane(&graph_pane) {
        log::warn!("failed to register H2 graph pane: {err:#}");
        return None;
    }
    if let Err(err) = mux.add_pane(&kanban_pane) {
        log::warn!("failed to register H2 kanban pane: {err:#}");
        return None;
    }

    tab.set_active_pane(&active);
    mux.notify(MuxNotification::TabResized(tab.tab_id()));

    let panes = DashboardPanes { graph, kanban };
    *DASHBOARD_PANES.lock().ok()? = Some(panes.clone());
    Some(panes)
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

fn graph_lines_from_snapshot(snapshot: &Value) -> Vec<String> {
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
    let mut lines = vec![
        "H2 Graph View".to_string(),
        format!("seq {seq}  live nodes {nodes}"),
        String::new(),
    ];

    if let Some(items) = snapshot.get("nodes").and_then(Value::as_array) {
        for item in items.iter().take(12) {
            let node = item
                .get("node")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let runtime = item
                .get("runtime")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            lines.push(format!("{node}  {runtime}"));
        }
    }

    lines
}

fn kanban_lines_from_snapshot(snapshot: &Value) -> Vec<String> {
    let artifact_count = snapshot
        .get("artifact_count")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let mut lines = vec![
        "H2 Kanban View".to_string(),
        format!("artifacts {artifact_count}"),
        String::new(),
    ];

    if let Some(items) = snapshot.get("slots").and_then(Value::as_array) {
        if !items.is_empty() {
            lines.push("Slots".to_string());
            for item in items.iter().take(6) {
                let slot = item
                    .get("slot")
                    .map(format_slot)
                    .unwrap_or_else(|| "unknown".to_string());
                let head = item
                    .get("head")
                    .and_then(Value::as_str)
                    .map(short_artifact_id)
                    .unwrap_or_else(|| "unknown".to_string());
                let seq = item.get("seq").and_then(Value::as_u64).unwrap_or_default();
                lines.push(format!("{slot}  {head}  seq {seq}"));
            }
            lines.push(String::new());
        }
    }

    lines.push("Events".to_string());

    if let Some(items) = snapshot.get("events").and_then(Value::as_array) {
        for item in items.iter().take(10) {
            let seq = item.get("seq").and_then(Value::as_u64).unwrap_or_default();
            let kind = item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            lines.push(format!("{seq}  {kind}"));
        }
    }

    lines.push(String::new());
    lines.push("Artifacts".to_string());

    if let Some(items) = snapshot.get("artifacts").and_then(Value::as_array) {
        for item in items.iter().take(8) {
            let seq = item.get("seq").and_then(Value::as_u64).unwrap_or_default();
            let kind = item
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let slot = item
                .get("slot")
                .map(format_slot)
                .unwrap_or_else(|| "unknown".to_string());
            lines.push(format!("{seq}  {kind}  {slot}"));
        }
    }

    lines
}

fn format_slot(slot: &Value) -> String {
    let node = slot
        .get("node")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let name = slot
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("{node}/{name}")
}

fn short_artifact_id(id: &str) -> String {
    id.chars().take(8).collect()
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

    #[test]
    fn graph_lines_show_live_nodes_from_snapshot() {
        let snapshot = serde_json::json!({
            "status": {
                "events_seq": 9,
                "registered_nodes": 2
            },
            "nodes": [
                {"node": "canvas", "runtime": "hterm-local-pane"},
                {"node": "agent-a", "runtime": "codex"}
            ]
        });

        assert_eq!(
            graph_lines_from_snapshot(&snapshot),
            vec![
                "H2 Graph View".to_string(),
                "seq 9  live nodes 2".to_string(),
                "".to_string(),
                "canvas  hterm-local-pane".to_string(),
                "agent-a  codex".to_string()
            ]
        );
    }

    #[test]
    fn kanban_lines_show_events_and_artifacts_from_snapshot() {
        let snapshot = serde_json::json!({
            "events": [
                {"seq": 8, "type": "node_registered"},
                {"seq": 9, "type": "trace_log"}
            ],
            "artifact_count": 1,
            "slots": [
                {
                    "slot": {"node": "canvas", "name": "note"},
                    "head": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "seq": 7
                }
            ],
            "artifacts": [
                {"seq": 7, "kind": "text", "slot": {"node": "canvas", "name": "note"}}
            ]
        });

        assert_eq!(
            kanban_lines_from_snapshot(&snapshot),
            vec![
                "H2 Kanban View".to_string(),
                "artifacts 1".to_string(),
                "".to_string(),
                "Slots".to_string(),
                "canvas/note  01234567  seq 7".to_string(),
                "".to_string(),
                "Events".to_string(),
                "8  node_registered".to_string(),
                "9  trace_log".to_string(),
                "".to_string(),
                "Artifacts".to_string(),
                "7  text  canvas/note".to_string()
            ]
        );
    }
}
