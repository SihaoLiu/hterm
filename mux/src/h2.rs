use crate::pane::PaneId;
use serde_json::json;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SOCKET_ENV: &str = "H2_NODE_SOCKET";

pub fn node_id_for_pane(pane_id: PaneId) -> String {
    format!("hterm-{}-pane-{pane_id}", std::process::id())
}

pub fn maybe_register_local_pane(pane_id: PaneId, command_description: String) {
    let Some(socket_path) = socket_path_from_env() else {
        return;
    };
    std::thread::spawn(move || {
        if let Err(err) = register_local_pane(&socket_path, pane_id, &command_description) {
            log::debug!(
                "failed to register h2 local pane {} via {}: {:#}",
                pane_id,
                socket_path.display(),
                err
            );
        }
    });
}

pub fn maybe_unregister_local_pane(pane_id: PaneId, reason: &'static str) {
    let Some(socket_path) = socket_path_from_env() else {
        return;
    };
    std::thread::spawn(move || {
        if let Err(err) = unregister_local_pane(&socket_path, pane_id, reason) {
            log::debug!(
                "failed to unregister h2 local pane {} via {}: {:#}",
                pane_id,
                socket_path.display(),
                err
            );
        }
    });
}

fn socket_path_from_env() -> Option<PathBuf> {
    std::env::var_os(SOCKET_ENV).map(PathBuf::from)
}

fn register_local_pane(
    socket_path: &Path,
    pane_id: PaneId,
    command_description: &str,
) -> anyhow::Result<()> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "register_node",
        "params": {
            "node": node_id_for_pane(pane_id),
            "runtime": "hterm-local-pane",
            "labels": {
                "pane_id": pane_id,
                "pid": std::process::id(),
                "command": command_description,
            },
        },
    });
    send_node_rpc(socket_path, &request)?;
    Ok(())
}

fn unregister_local_pane(
    socket_path: &Path,
    pane_id: PaneId,
    reason: &'static str,
) -> anyhow::Result<()> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "unregister_node",
        "params": {
            "node": node_id_for_pane(pane_id),
            "reason": reason,
        },
    });
    send_node_rpc(socket_path, &request)?;
    Ok(())
}

fn send_node_rpc(
    socket_path: &Path,
    request: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(socket_path)?;
        let timeout = Some(Duration::from_millis(750));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;

        let payload = serde_json::to_vec(request)?;
        stream.write_all(&(payload.len() as u32).to_be_bytes())?;
        stream.write_all(&payload)?;
        stream.flush()?;

        let mut len = [0u8; 4];
        stream.read_exact(&mut len)?;
        let mut response = vec![0u8; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut response)?;
        let response: serde_json::Value = serde_json::from_slice(&response)?;
        if let Some(error) = response.get("error").filter(|v| !v.is_null()) {
            anyhow::bail!("node-rpc error: {error}");
        }
        Ok(response)
    }

    #[cfg(not(unix))]
    {
        let _ = socket_path;
        let _ = request;
        anyhow::bail!("h2 node-rpc bridge requires unix sockets")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_for_pane_includes_process_and_pane_identity() {
        let node_id = node_id_for_pane(7);
        let pid = std::process::id();

        assert_eq!(node_id, format!("hterm-{pid}-pane-7"));
    }
}
