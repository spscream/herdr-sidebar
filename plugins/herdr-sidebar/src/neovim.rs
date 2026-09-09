//! Hand a clicked file to the `chmarax.herdr-nvim` plugin's sidebar instead
//! of the builtin viewer, when [`crate::state::PreviewEditor::Neovim`] is
//! selected.
//!
//! The file opens in a preview tab of its own, the way the builtin previewer's
//! tab placement does: one tab per space, reused by every later click, with
//! nvim filling it and the explorer docked beside it.
//!
//! herdr-nvim ships a small `open-file <path>` CLI (its own
//! `bridge::open_in_sidebar`, the same entrypoint `pick-file` and the
//! Ctrl+click link handler use) that opens the tab's nvim daemon, ensures the
//! sidebar pane, and jumps to the file. It reads the tab it is to work on from
//! `HERDR_TAB_ID`, `HERDR_PANE_ID` and `HERDR_WORKSPACE_ID`, so aiming it at
//! the preview tab needs nothing from herdr-nvim itself.
//!
//! Reuse is what makes this cheap. The first click builds the tab and starts
//! that tab's nvim daemon. Every later click reaches a tab that already holds
//! a live nvim sidebar, so herdr-nvim opens the file as another buffer and
//! changes no layout at all -- one nvim per space, with a shared jumplist,
//! shared marks and shared search history.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use crate::ipc;

const PLUGIN_ID: &str = "chmarax.herdr-nvim";

/// The label herdr-nvim gives its sidebar pane. It comes from the `title` of
/// the `sidebar` pane in that plugin's manifest, and it is how a tab is
/// recognised as one of ours: a tab holding a pane with this label already
/// has an nvim to open the next file in.
const NVIM_PANE_LABEL: &str = "nvim sidebar";

/// The suffix the builtin previewer puts on its tab labels
/// (`viewer::preview_pane_label`). Reused here so a preview tab reads the same
/// whichever editor drew it, and so an ordinary working tab that happens to
/// hold an nvim sidebar -- one the user opened with herdr-nvim's own toggle --
/// is never mistaken for a preview tab and taken over.
const PREVIEW_SUFFIX: &str = " · preview";

/// Open `path` in an nvim preview tab.
///
/// Returns as soon as the failures worth reporting are ruled out. Everything
/// past that point runs on a thread of its own, because it is slow: starting a
/// tab's nvim daemon was measured at about 0.9 s cold, and the whole first
/// click at about 2 s. This function is called from the TUI's own event-loop
/// thread, where the launcher closes any pane that misses its heartbeat for
/// 20 s. See [`crate::actions::open_external`] for the same reasoning.
///
/// The price of not waiting is that the work on that thread cannot report
/// anything, so every failure this function can name has to be caught before
/// the thread starts: no plugin, no binary, no file, no pane id. A failure
/// past that point is silent, and the file simply does not appear.
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
    let me = std::env::var("HERDR_PANE_ID")
        .map_err(|_| "HERDR_PANE_ID is not set; cannot tell which space to preview in".to_string())?;
    let path = path.to_path_buf();
    std::thread::spawn(move || show(&binary, &me, &path));
    Ok(())
}

/// Put `path` on the space's preview tab, building that tab if this is the
/// first click. Best-effort throughout: a step that fails leaves the tab as it
/// was, and the next click tries again.
fn show(binary: &Path, my_pane: &str, path: &Path) {
    let Ok(list) = ipc::call_text("pane.list", serde_json::json!({})) else {
        return;
    };
    let workspace = crate::launch::workspace_of(&list, my_pane);
    if workspace.is_empty() {
        return;
    }
    let tabs = ipc::call_text("tab.list", serde_json::json!({})).unwrap_or_default();

    // Building the tab has to finish before an Explorer is docked into it, and
    // the ensure lock is what orders the two. Our own auto-dock hook fires on
    // the tab creation below; unheld, it lands while the tab still holds only
    // the scratch pane, and herdr-nvim then finds a two-pane tab, evacuates it
    // and rebuilds it -- which loses both the dock side and the width, and
    // leaves the Explorer at herdr's default half split. Held, the hook runs
    // after the scratch pane is gone, so it docks into a tab holding nothing
    // but nvim: one pane, no maneuver, `dock_right` and `sidebar_width` kept.
    let (tab, pane, scratch, lock) = match preview_tab_in(&list, &tabs, &workspace) {
        Some((tab, pane)) => (tab, pane, None, None),
        None => {
            let lock = crate::ensure::Lock::acquire(true);
            let Some((tab, pane)) = build_preview_tab(my_pane) else {
                return;
            };
            (tab, pane.clone(), Some(pane), lock)
        }
    };

    let opened = run_open_file(binary, &workspace, &tab, &pane, path, lock.is_some());

    // The pane the new tab was built from has done its job: herdr-nvim split
    // the nvim pane off it. Closing it before the split would have emptied the
    // tab, and herdr closes an empty tab.
    if let Some(scratch) = scratch {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": scratch }));
    }
    // Everything that rearranges the tab is done; release the hook.
    drop(lock);
    if !opened {
        return;
    }
    let _ = ipc::call_text(
        "tab.rename",
        serde_json::json!({ "tab_id": tab, "label": preview_tab_label(path) }),
    );
    let _ = ipc::call_text("tab.focus", serde_json::json!({ "tab_id": tab }));
}

/// Build the space's preview tab: split a pane off the explorer and move that
/// one pane into a tab of its own.
///
/// A pane is moved rather than a tab created because `tab.create` seeds the
/// new tab with a shell of its own, which would have to be closed later and
/// shows a prompt in the meantime. `pane.move` to a new tab produces a tab
/// with exactly the pane given to it. The builtin previewer builds its tab
/// the same way (`viewer::spawn_preview_tab`).
fn build_preview_tab(my_pane: &str) -> Option<(String, String)> {
    let split = ipc::call_text(
        "pane.split",
        serde_json::json!({
            "target_pane_id": my_pane,
            "direction": "right",
            "ratio": 0.5,
            "focus": false,
        }),
    )
    .ok()?;
    let pane = crate::launch::split_pane_id(&split)?;
    let moved = ipc::call_text(
        "pane.move",
        serde_json::json!({
            "pane_id": pane,
            "destination": { "type": "new_tab" },
            "focus": false,
        }),
    );
    if moved.is_err() {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane }));
        return None;
    }
    let list = ipc::call_text("pane.list", serde_json::json!({})).ok()?;
    let tab = crate::launch::tab_of(&list, &pane);
    if tab.is_empty() {
        let _ = ipc::call_text("pane.close", serde_json::json!({ "pane_id": pane }));
        return None;
    }
    Some((tab, pane))
}

/// Run herdr-nvim's `open-file` against one specific tab, and wait for it.
///
/// This runs on the spawned thread, never on the event loop. Waiting is what
/// makes the steps after it safe: the pane the tab was built from may only be
/// closed once nvim has split off it, and the tab may only be renamed once
/// there is something in it to name.
fn run_open_file(
    binary: &Path,
    workspace: &str,
    tab: &str,
    pane: &str,
    path: &Path,
    holding_lock: bool,
) -> bool {
    let mut cmd = std::process::Command::new(binary);
    // herdr-nvim takes the same ensure lock for the length of its maneuver, to
    // keep our hook out of the gap where the tab has no Explorer. When we
    // already hold it, telling it so saves it waiting out our own guard.
    if holding_lock {
        cmd.env("HERDR_SIDEBAR_ENSURE_LOCK_HELD", "1");
    }
    cmd.arg("open-file")
        .arg(path)
        .env("HERDR_WORKSPACE_ID", workspace)
        .env("HERDR_TAB_ID", tab)
        .env("HERDR_PANE_ID", pane)
        // The child and its grandchildren write to whatever they inherit:
        // herdr answers `plugin pane focus` with a JSON object, nvim's
        // `--remote-expr` prints its result. Inherited, that text lands in
        // this TUI's screen buffer, where ratatui's diff will not repaint over
        // it.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The tab label for a previewed file, matching the builtin previewer's shape.
fn preview_tab_label(path: &Path) -> String {
    let name = path
        .file_name()
        .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into());
    format!("{name}{PREVIEW_SUFFIX}")
}

/// The space's preview tab and one pane in it, if a previous click already
/// built it: a tab whose label carries the preview suffix and that holds a
/// live nvim sidebar pane.
///
/// Both halves are required. The suffix alone would adopt a builtin-viewer
/// preview tab, which has no nvim in it. The nvim pane alone would take over
/// a working tab in which the user opened herdr-nvim's sidebar by hand, and
/// then rename it.
fn preview_tab_in(pane_list_json: &str, tab_list_json: &str, workspace: &str) -> Option<(String, String)> {
    let panes: serde_json::Value = serde_json::from_str(pane_list_json).ok()?;
    let panes = panes.pointer("/result/panes")?.as_array()?;
    let tabs: serde_json::Value = serde_json::from_str(tab_list_json).ok()?;
    let tabs = tabs.pointer("/result/tabs")?.as_array()?;

    let field = |value: &serde_json::Value, key: &str| -> String {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    for tab in tabs {
        if field(tab, "workspace_id") != workspace {
            continue;
        }
        if !field(tab, "label").ends_with(PREVIEW_SUFFIX) {
            continue;
        }
        let tab_id = field(tab, "tab_id");
        let nvim = panes
            .iter()
            .find(|pane| field(pane, "tab_id") == tab_id && field(pane, "label") == NVIM_PANE_LABEL);
        if let Some(nvim) = nvim {
            return Some((tab_id, field(nvim, "pane_id")));
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    const PANES: &str = r#"{"result":{"panes":[
        {"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","label":"Sidebar"},
        {"pane_id":"w1:p9","tab_id":"w1:t9","workspace_id":"w1","label":"nvim sidebar"},
        {"pane_id":"w1:p8","tab_id":"w1:t8","workspace_id":"w1","label":"nvim sidebar"},
        {"pane_id":"w1:p7","tab_id":"w1:t7","workspace_id":"w1","label":"Sidebar"},
        {"pane_id":"w2:p9","tab_id":"w2:t9","workspace_id":"w2","label":"nvim sidebar"}
    ]}}"#;

    const TABS: &str = r#"{"result":{"tabs":[
        {"tab_id":"w1:t1","workspace_id":"w1","label":"1"},
        {"tab_id":"w1:t8","workspace_id":"w1","label":"3"},
        {"tab_id":"w1:t7","workspace_id":"w1","label":"notes.md · preview"},
        {"tab_id":"w1:t9","workspace_id":"w1","label":"AGENTS.md · preview"},
        {"tab_id":"w2:t9","workspace_id":"w2","label":"other.rs · preview"}
    ]}}"#;

    #[test]
    fn the_preview_tab_is_the_one_with_both_the_suffix_and_an_nvim() {
        assert_eq!(
            preview_tab_in(PANES, TABS, "w1"),
            Some(("w1:t9".to_string(), "w1:p9".to_string()))
        );
    }

    #[test]
    fn a_working_tab_holding_an_nvim_sidebar_is_not_adopted() {
        // w1:t8 has the nvim pane but an ordinary numbered label: the user
        // opened herdr-nvim's sidebar there by hand.
        let tabs = r#"{"result":{"tabs":[
            {"tab_id":"w1:t8","workspace_id":"w1","label":"3"}
        ]}}"#;
        assert_eq!(preview_tab_in(PANES, tabs, "w1"), None);
    }

    #[test]
    fn a_builtin_viewers_preview_tab_is_not_adopted() {
        // w1:t7 carries the suffix but holds the builtin viewer, not an nvim.
        let tabs = r#"{"result":{"tabs":[
            {"tab_id":"w1:t7","workspace_id":"w1","label":"notes.md · preview"}
        ]}}"#;
        assert_eq!(preview_tab_in(PANES, tabs, "w1"), None);
    }

    #[test]
    fn another_spaces_preview_tab_is_never_borrowed() {
        // The same tab, asked for from w1, must not answer w2's tab -- a
        // session-wide search made focus jump to another project.
        assert_eq!(
            preview_tab_in(PANES, TABS, "w2"),
            Some(("w2:t9".to_string(), "w2:p9".to_string()))
        );
        let only_w2 = r#"{"result":{"tabs":[
            {"tab_id":"w2:t9","workspace_id":"w2","label":"other.rs · preview"}
        ]}}"#;
        assert_eq!(preview_tab_in(PANES, only_w2, "w1"), None);
    }

    #[test]
    fn tab_label_is_the_file_name_with_the_shared_suffix() {
        assert_eq!(
            preview_tab_label(Path::new("/repo/src/main.rs")),
            "main.rs · preview"
        );
    }

    #[test]
    fn garbage_json_yields_no_tab_rather_than_a_panic() {
        assert_eq!(preview_tab_in("not json", TABS, "w1"), None);
        assert_eq!(preview_tab_in(PANES, "not json", "w1"), None);
    }
}
