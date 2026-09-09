//! Hand a clicked file to the `chmarax.herdr-nvim` plugin's sidebar instead
//! of the builtin viewer, when [`crate::state::PreviewEditor::Neovim`] is
//! selected. herdr-nvim ships a small `open-file <path>` CLI (its own
//! `bridge::open_in_sidebar`, the same entrypoint `pick-file` and the
//! Ctrl+click link handler use) that opens the tab's nvim daemon, ensures the
//! sidebar pane, and jumps to the file — so this module only needs to find
//! that binary and run it.

use std::path::Path;

const PLUGIN_ID: &str = "chmarax.herdr-nvim";

/// Open `path` in the herdr-nvim sidebar. Best-effort: the caller falls back
/// to nothing on error (the click just fails with a notice) rather than
/// silently opening the builtin viewer, so a broken/missing install is
/// visible instead of masked.
pub fn open(path: &Path) -> Result<(), String> {
    let root = plugin_root()?;
    let binary = root.join("bin").join("herdr-nvim");
    let status = std::process::Command::new(&binary)
        .arg("open-file")
        .arg(path)
        .status()
        .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
    if !status.success() {
        return Err(format!("herdr-nvim open-file exited with {status}"));
    }
    Ok(())
}

/// Look up the installed `chmarax.herdr-nvim` plugin's root directory over
/// herdr's socket API (`plugin.list`), the same protocol [`crate::ipc`] uses
/// for pane control.
fn plugin_root() -> Result<std::path::PathBuf, String> {
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
    Ok(std::path::PathBuf::from(root))
}
