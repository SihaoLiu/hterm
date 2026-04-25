use crate::domain::DomainId;
use crate::pane::{
    alloc_pane_id, CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId,
    Pattern, PerformAssignmentResult, WithPaneLines,
};
use crate::renderable::{RenderableDimensions, StableCursorPosition};
use config::keyassignment::{KeyAssignment, ScrollbackEraseMode};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use rangeset::RangeSet;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use termwiz::hyperlink::Rule;
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{CursorVisibility, Line, SequenceNo};
use url::Url;
use wezterm_dynamic::{Object, Value};
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress, SemanticZone,
    StableRowIndex, TerminalConfiguration, TerminalSize,
};

const SOCKET_ENV: &str = "H2_NODE_SOCKET";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H2PaneKind {
    Graph,
    Kanban,
}

impl H2PaneKind {
    pub fn title(self) -> &'static str {
        match self {
            H2PaneKind::Graph => "H2 Graph",
            H2PaneKind::Kanban => "H2 Kanban",
        }
    }
}

pub struct H2Pane {
    pane_id: PaneId,
    kind: H2PaneKind,
    size: Mutex<TerminalSize>,
    source_lines: Mutex<Vec<String>>,
    rendered_lines: Mutex<Vec<Line>>,
    seqno: AtomicUsize,
    dead: AtomicBool,
    writer_sink: Mutex<Vec<u8>>,
    graph_node_count: AtomicUsize,
}

impl H2Pane {
    pub fn new(kind: H2PaneKind, size: TerminalSize) -> Arc<Self> {
        let pane = Arc::new(Self {
            pane_id: alloc_pane_id(),
            kind,
            size: Mutex::new(size),
            source_lines: Mutex::new(vec![kind.title().to_string()]),
            rendered_lines: Mutex::new(Vec::new()),
            seqno: AtomicUsize::new(1),
            dead: AtomicBool::new(false),
            writer_sink: Mutex::new(Vec::new()),
            graph_node_count: AtomicUsize::new(0),
        });
        pane.rebuild_lines();
        pane
    }

    pub fn kind(&self) -> H2PaneKind {
        self.kind
    }

    pub fn set_lines(&self, lines: Vec<String>) {
        *self.source_lines.lock() = lines;
        self.rebuild_lines();
    }

    pub fn set_graph_node_count(&self, count: usize) {
        self.graph_node_count.store(count, Ordering::Relaxed);
        self.seqno.fetch_add(1, Ordering::Relaxed);
    }

    fn rebuild_lines(&self) {
        let size = *self.size.lock();
        let rows = size.rows.max(1);
        let cols = size.cols.max(1);
        let source = self.source_lines.lock().clone();
        let mut rendered = Vec::with_capacity(rows);

        for row in 0..rows {
            let text = source.get(row).map(String::as_str).unwrap_or("");
            let clipped: String = text.chars().take(cols).collect();
            rendered.push(Line::from_text(&clipped, &Default::default(), 1, None));
        }

        *self.rendered_lines.lock() = rendered;
        self.seqno.fetch_add(1, Ordering::Relaxed);
    }
}

#[async_trait::async_trait(?Send)]
impl Pane for H2Pane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        StableCursorPosition {
            visibility: CursorVisibility::Hidden,
            ..StableCursorPosition::default()
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.seqno.load(Ordering::Relaxed)
    }

    fn get_metadata(&self) -> Value {
        let mut obj = BTreeMap::new();
        obj.insert(
            Value::String("h2_pane_kind".into()),
            Value::String(
                match self.kind {
                    H2PaneKind::Graph => "graph",
                    H2PaneKind::Kanban => "kanban",
                }
                .into(),
            ),
        );
        obj.insert(
            Value::String("h2_graph_node_count".into()),
            Value::U64(self.graph_node_count.load(Ordering::Relaxed) as u64),
        );
        Value::Object(Object::from(obj))
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut set = RangeSet::new();
        if self.get_current_seqno() != seqno {
            set.add_range(lines);
        }
        set
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let rendered = self.rendered_lines.lock();
        let first = lines.start.max(0);
        let out = (lines.start..lines.end)
            .map(|idx| {
                rendered
                    .get(idx.max(0) as usize)
                    .cloned()
                    .unwrap_or_else(|| Line::new(0))
            })
            .collect();
        (first, out)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        let rendered = self.rendered_lines.lock();
        let mut local: Vec<Line> = (lines.start..lines.end)
            .filter(|idx| *idx >= 0)
            .map(|idx| {
                rendered
                    .get(idx as usize)
                    .cloned()
                    .unwrap_or_else(|| Line::new(0))
            })
            .collect();
        let mut refs: Vec<&mut Line> = local.iter_mut().collect();
        with_lines.with_lines_mut(lines.start.max(0), &mut refs);
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        crate::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, for_line)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn apply_hyperlinks(&self, _lines: Range<StableRowIndex>, _rules: &[Rule]) {}

    fn get_dimensions(&self) -> RenderableDimensions {
        let size = *self.size.lock();
        RenderableDimensions {
            cols: size.cols,
            viewport_rows: size.rows,
            scrollback_rows: size.rows,
            physical_top: 0,
            scrollback_top: 0,
            dpi: size.dpi,
            pixel_width: size.pixel_width,
            pixel_height: size.pixel_height,
            reverse_video: false,
        }
    }

    fn get_title(&self) -> String {
        self.kind.title().to_string()
    }

    fn get_progress(&self) -> Progress {
        Progress::None
    }

    fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        MutexGuard::map(self.writer_sink.lock(), |sink| {
            sink as &mut dyn std::io::Write
        })
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        *self.size.lock() = size;
        self.rebuild_lines();
        Ok(())
    }

    fn set_zoomed(&self, _zoomed: bool) {}

    fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn perform_assignment(&self, _assignment: &KeyAssignment) -> PerformAssignmentResult {
        PerformAssignmentResult::Unhandled
    }

    fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
        Ok(())
    }

    fn perform_actions(&self, _actions: Vec<termwiz::escape::Action>) {}

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    fn kill(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }

    fn palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn domain_id(&self) -> DomainId {
        0
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        KeyboardEncoding::Xterm
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    fn erase_scrollback(&self, _erase_mode: ScrollbackEraseMode) {}

    fn focus_changed(&self, _focused: bool) {}

    fn advise_focus(&self) {}

    fn has_unseen_output(&self) -> bool {
        false
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        true
    }

    async fn search(
        &self,
        _pattern: Pattern,
        _range: Range<StableRowIndex>,
        _limit: Option<u32>,
    ) -> anyhow::Result<Vec<crate::pane::SearchResult>> {
        Ok(Vec::new())
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        Ok(Vec::new())
    }

    fn is_mouse_grabbed(&self) -> bool {
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn set_clipboard(&self, _clipboard: &Arc<dyn Clipboard>) {}

    fn set_download_handler(&self, _handler: &Arc<dyn DownloadHandler>) {}

    fn set_config(&self, _config: Arc<dyn TerminalConfiguration>) {}

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        None
    }

    fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
        None
    }

    fn get_foreground_process_name(&self, _policy: CachePolicy) -> Option<String> {
        None
    }

    fn get_foreground_process_info(
        &self,
        _policy: CachePolicy,
    ) -> Option<procinfo::LocalProcessInfo> {
        None
    }

    fn tty_name(&self) -> Option<String> {
        None
    }

    fn exit_behavior(&self) -> Option<crate::ExitBehavior> {
        None
    }
}

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

    #[test]
    fn h2_pane_renders_text_lines() {
        let pane = H2Pane::new(
            H2PaneKind::Kanban,
            wezterm_term::TerminalSize {
                rows: 4,
                cols: 24,
                pixel_width: 240,
                pixel_height: 80,
                dpi: 96,
            },
        );

        pane.set_lines(vec!["H2 Kanban".into(), "events: 3".into()]);

        let (_first, lines) = pane.get_lines(0..4);
        let text: Vec<_> = lines.iter().map(|line| line.as_str().to_string()).collect();

        assert_eq!(text, vec!["H2 Kanban", "events: 3", "", ""]);
        assert_eq!(pane.get_title(), "H2 Kanban");
        assert_eq!(pane.get_dimensions().viewport_rows, 4);
    }
}
