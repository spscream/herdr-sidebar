//! Hand a clicked file to the `chmarax.herdr-nvim` plugin's sidebar instead
//! of the builtin viewer, when [`crate::state::PreviewEditor::Neovim`] is
//! selected. herdr-nvim ships a small `open-file <path>` CLI (its own
//! `bridge::open_in_sidebar`, the same entrypoint `pick-file` and the
//! Ctrl+click link handler use) that opens the tab's nvim daemon, ensures the
//! sidebar pane, and jumps to the file — so this module only needs to find
//! that binary and run it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

const PLUGIN_ID: &str = "chmarax.herdr-nvim";

/// Open `path` in the herdr-nvim sidebar.
///
/// The child is spawned, never waited for. It has real work to do — start the
/// tab's nvim daemon (measured at ~0.9 s cold), open the sidebar pane, relay
/// out the tab — and this runs on the TUI's own event-loop thread, so waiting
/// would freeze the sidebar for the whole of it. Worse than a freeze: the
/// launcher treats a pane that misses its heartbeat for 20 s as dead and
/// closes it. See [`crate::actions::open_external`] for the same pattern.
///
/// The price of not waiting is that the child's exit code is unreadable, so
/// every failure this function can report has to be caught before the spawn.
/// Those are the failures that actually happen: no plugin, no binary, no
/// file. A failure past that point is silent, and the file simply does not
/// appear.
pub fn open(path: &Path) -> Result<(), String> {
    // Check the path here as well as in herdr-nvim: this side is the one that
    // can still put a notice on the screen.
    if !path.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }
    let root = plugin_root()?;
    let binary = root.join("bin").join("herdr-nvim");
    if !binary.is_file() {
        return Err(format!("{PLUGIN_ID} has no binary at {}", binary.display()));
    }
    std::process::Command::new(&binary)
        .arg("open-file")
        .arg(path)
        // The child and its grandchildren write to whatever they inherit:
        // herdr answers `plugin pane focus` with a JSON object, nvim's
        // `--remote-expr` prints its result. Inherited, that text lands in
        // this TUI's screen buffer, where ratatui's diff will not repaint over
        // it.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
    Ok(())
}

/// Look up the installed `chmarax.herdr-nvim` plugin's root directory over
/// herdr's socket API (`plugin.list`), the same protocol [`crate::ipc`] uses
/// for pane control.
///
/// Cached after the first success. The call is a socket round trip with a
/// timeout of several seconds, and it runs on the event-loop thread; the
/// answer is a plugin's install directory, which does not move while the
/// sidebar is running. A failure is not cached, so a plugin installed mid-
/// session is picked up by the next click.
fn plugin_root() -> Result<PathBuf, String> {
    static CACHE: OnceLock<PathBuf> = OnceLock::new();
    if let Some(root) = CACHE.get() {
        return Ok(root.clone());
    }
    let root = query_plugin_root()?;
    Ok(CACHE.get_or_init(|| root).clone())
}

fn query_plugin_root() -> Result<PathBuf, String> {
    let response = crate::ipc::call_text("plugin.list", serde_json::json!({}))
        .map_err(|e| format!("cannot reach herdr: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&response).map_err(|e| format!("bad plugin.list response: {e}"))?;
    let plugins = value
        .get("result")
        .and_then(|r| r.get("plugins"))
        .and_then(|p| p.as_array())
        .ok_or("plugin.list response missing plugins array")?;
    let root = plugins
        .iter()
        .find(|p| p.get("plugin_id").and_then(|id| id.as_str()) == Some(PLUGIN_ID))
        .and_then(|p| p.get("plugin_root"))
        .and_then(|r| r.as_str())
        .ok_or_else(|| format!("{PLUGIN_ID} is not installed"))?;
    Ok(PathBuf::from(root))
}
