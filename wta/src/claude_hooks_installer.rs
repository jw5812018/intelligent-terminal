// wta/src/claude_hooks_installer.rs
//
// Auto-install the wt-agent-hooks bridge into Claude Code AND Copilot CLI
// on wta startup.
//
// Why this exists
// ===============
//
// The wta agent-pane registry transitions a session out of `IDLE` only when
// it receives `agent_event` broadcasts from the COM server. Those events
// originate from a small PowerShell bridge (`send-event.ps1`) that the
// CLI invokes through its hook system. If the user hasn't run a manual
// plugin-install step, the CLI never invokes the bridge, the registry
// stays empty, and the F2 list looks frozen.
//
// Each supported CLI loads hooks differently, so this module installs the
// bridge through the mechanism each CLI actually honors:
//
//   * Claude Code reads hook definitions directly from the top-level
//     `hooks` object in `~/.claude/settings.json`. We merge a tagged
//     block in there pointing at our bridge script. Idempotent.
//
//   * Copilot CLI does NOT honor a top-level `hooks` block — only plugins
//     registered through its plugin manager have their `hooks/hooks.json`
//     files loaded. Copilot's plugin manager has two strict requirements
//     that took us several rounds to get right:
//
//       1. The plugin manifest must live at `<plugin-root>/.claude-plugin/
//          plugin.json` (NOT at the plugin root). A bare `plugin.json`
//          at the root is silently ignored.
//
//       2. The plugin must be discoverable through a registered
//          *marketplace* whose name passes Copilot's kebab-case validator
//          (letters, digits, hyphens — no underscores). Earlier builds
//          tried the marketplace name `_direct`, which Copilot rejects:
//          `Invalid marketplace.json: name: Marketplace name must be
//          kebab-case`. We use `wt-local`.
//
//     The full layout we deploy:
//
//       ~/.copilot/installed-plugins/wt-local/
//         .claude-plugin/
//           marketplace.json    # lists wt-agent-hooks
//         agent-hooks-plugin/
//           .claude-plugin/
//             plugin.json       # plugin manifest
//           hooks/
//             hooks.json        # generated from HOOK_EVENTS
//             send-event.ps1    # embedded bridge script
//
//     And these settings.json edits:
//       extraKnownMarketplaces.wt-local = {
//         source: { source: "directory", path: "<abs path to wt-local>" }
//       }
//       installedPlugins[] += { name: "wt-agent-hooks", marketplace: "wt-local",
//                                version, enabled: true, cache_path, installed_at }
//       enabledPlugins["wt-agent-hooks@wt-local"] = true
//
//     This is the exact layout `copilot plugin marketplace add <local>`
//     followed by `copilot plugin install wt-agent-hooks@wt-local`
//     produces — we replicate the file/JSON output without spawning the
//     CLI (so it works at wta startup before Copilot is even launched).
//
// In all cases the bridge script itself is shared: we write the embedded
// copy once to `%LOCALAPPDATA%\IntelligentTerminal\hooks\send-event.ps1`
// (Claude points there directly via absolute path) and also drop a copy
// inside the Copilot plugin folder so `${CLAUDE_PLUGIN_ROOT}` resolution
// from Copilot's own hooks.json finds it.
//
// Cleanup of older formats: this module also strips
//   * Top-level wta-tagged entries from `~/.copilot/settings.json` (round-5
//     mistake — Copilot ignored those entries anyway).
//   * `installedPlugins[]` and `enabledPlugins{}` entries with
//     `marketplace == "_direct"` (round-6 mistake — failed kebab-case
//     validator and never loaded).
//   * Stale `~/.copilot/installed-plugins/_direct/` folder.
//
// All writes are best-effort: failures are logged but do not block startup.
// On every startup the merge logic re-checks the on-disk content and only
// rewrites when something has drifted.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// String used to tag every hook entry we manage so we can re-detect them
/// across runs and avoid duplicating entries on each wta launch.
const WTA_TAG: &str = "wt-agent-hooks";

/// Plugin name used in the Copilot plugin manifest and the
/// `enabledPlugins` map key. Must match `plugin.json` `name`.
const COPILOT_PLUGIN_NAME: &str = "wt-agent-hooks";

/// Marketplace identifier under which our plugin lives. Copilot CLI requires
/// marketplace names to be kebab-case (letters, numbers, hyphens — no
/// underscores). Used as:
///   * Folder name under `installed-plugins/<marketplace>/`.
///   * Key in `extraKnownMarketplaces` in settings.json.
///   * Suffix on `enabledPlugins` map keys (`<plugin>@<marketplace>`).
///
/// Older wta builds used `_direct` here, which Copilot CLI silently rejected
/// as a marketplace name (failing the kebab-case validator), causing the
/// plugin to never load even when the folder existed on disk.
const COPILOT_MARKETPLACE_NAME: &str = "wt-local";

/// Folder name under the marketplace folder that holds the plugin itself.
/// Copilot CLI's `plugin install` resolves the source path from
/// marketplace.json, then **copies** the plugin into a folder named after
/// the plugin's `name` field — so the canonical install destination is
/// `wt-local/<plugin-name>/`. We skip the source-folder copy step and
/// write the plugin directly to the canonical location, matching what
/// `copilot plugin list` validates against `installedPlugins[].cache_path`.
const COPILOT_PLUGIN_DIR_NAME: &str = COPILOT_PLUGIN_NAME;

/// Plugin version string written into `installedPlugins[].version`,
/// `plugin.json`, and `marketplace.json`. Bumped only when the wire format /
/// hook surface changes in a way users need to notice.
const COPILOT_PLUGIN_VERSION: &str = "0.1.0";

/// Embedded copy of the bridge script. Sourced from
/// `wta/agent-hooks-plugin/hooks/send-event.ps1` at build time.
const SEND_EVENT_PS1: &str = include_str!("../agent-hooks-plugin/hooks/send-event.ps1");

/// Human-readable description used in both `plugin.json` and
/// `marketplace.json`. Kept short on purpose — Copilot CLI surfaces this
/// in `copilot plugin list` output.
const COPILOT_PLUGIN_DESCRIPTION: &str =
    "Forward CLI agent hook events to Windows Terminal for WTA display";

/// Hook event names → wta-side event-type identifier passed to the script.
/// Order mirrors `wta/agent-hooks-plugin/hooks/hooks.json` so the on-disk
/// behavior matches what a plugin install would have produced.
///
/// Only events Claude recognizes natively are listed here. Unknown event
/// names cause Claude to surface a "Quick safety check" warning at startup
/// asking the user how to handle the malformed settings.json — that's
/// hostile UX, so we keep this list strictly within Claude's documented
/// catalog (https://code.claude.com/docs/en/hooks). Copilot CLI accepts
/// the same set (a subset of the Claude format), so we reuse the table.
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart",      "agent.session.start"),
    ("SessionEnd",        "agent.session.end"),
    ("Notification",      "agent.notification"),
    ("UserPromptSubmit",  "agent.prompt.submit"),
    ("PreToolUse",        "agent.tool.starting"),
    ("PostToolUse",       "agent.tool.finished"),
    ("Stop",              "agent.stop"),
    ("SubagentStop",      "agent.subagent.stop"),
];

/// Top-level entry point. Run once at wta startup. Idempotent and silent on
/// failure: if a CLI isn't installed, we skip it; if its settings.json is
/// malformed, we leave it alone.
pub fn ensure_installed() {
    let Some(home) = home_dir() else {
        tracing::debug!(target: "claude_hooks", "no HOME/USERPROFILE; skipping");
        return;
    };
    ensure_installed_in(&home);
}

/// Run the installer against a specific home directory. Split out from
/// `ensure_installed` so tests can drive it with an isolated tempdir
/// without mutating `USERPROFILE`/`HOME` for the whole process.
fn ensure_installed_in(home: &Path) {
    // Write the bridge script once — it's shared across all CLIs that point
    // at the LOCALAPPDATA copy via absolute path.
    let shared_script_path = match write_bridge_script() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(target: "claude_hooks", err = %e, "failed to write shared bridge script");
            return;
        }
    };

    install_for_claude(home, &shared_script_path);
    install_for_copilot(home);
}

/// Install hooks for Claude Code by merging a tagged `hooks` block into
/// `~/.claude/settings.json` that points at the shared bridge script.
fn install_for_claude(home: &Path, shared_script_path: &Path) {
    let claude_dir = home.join(".claude");
    if !claude_dir.is_dir() {
        tracing::debug!(target: "claude_hooks", "no ~/.claude dir; Claude not present");
        return;
    }
    let settings_path = claude_dir.join("settings.json");
    if let Err(e) = ensure_hooks_in_settings(&settings_path, shared_script_path, "claude_hooks") {
        tracing::warn!(target: "claude_hooks", err = %e, "failed to update settings.json");
    }
}

/// Install hooks for Copilot CLI by deploying the bridge as a marketplace
/// plugin under `~/.copilot/installed-plugins/wt-local/wt-agent-hooks/`
/// and registering it across `~/.copilot/settings.json` (marketplace +
/// enabled state) and `~/.copilot/config.json` (installedPlugins[] —
/// what `copilot plugin list` actually reads).
///
/// **Why two settings files:** Copilot CLI uses `config.json` as its
/// auto-managed cache (it's the file `copilot plugin install` writes
/// `installedPlugins[]` to), and `settings.json` for user-toggleable
/// state (`enabledPlugins`, `extraKnownMarketplaces`). The plugin will
/// only appear in `copilot plugin list` if `config.json.installedPlugins[]`
/// has a matching entry whose `cache_path` resolves to a real folder —
/// writing the entry only into settings.json (as round-6/-7 wta did) is
/// silently ignored.
fn install_for_copilot(home: &Path) {
    let copilot_dir = home.join(".copilot");
    if !copilot_dir.is_dir() {
        tracing::debug!(target: "copilot_hooks", "no ~/.copilot dir; Copilot CLI not present");
        return;
    }

    // Marketplace folder: `~/.copilot/installed-plugins/wt-local/`. The
    // plugin folder sits inside it, named after the plugin (matches the
    // canonical layout `copilot plugin install` produces).
    let marketplace_dir = copilot_dir
        .join("installed-plugins")
        .join(COPILOT_MARKETPLACE_NAME);
    let plugin_dir = marketplace_dir.join(COPILOT_PLUGIN_DIR_NAME);

    if let Err(e) = write_copilot_marketplace_files(&marketplace_dir) {
        tracing::warn!(
            target: "copilot_hooks",
            err = %e,
            "failed to write marketplace.json",
        );
        return;
    }
    if let Err(e) = write_copilot_plugin_files(&plugin_dir) {
        tracing::warn!(
            target: "copilot_hooks",
            err = %e,
            "failed to write plugin folder",
        );
        return;
    }

    let settings_path = copilot_dir.join("settings.json");
    if let Err(e) = register_copilot_plugin_in_settings(&settings_path, &marketplace_dir) {
        tracing::warn!(
            target: "copilot_hooks",
            err = %e,
            "failed to register plugin in settings.json",
        );
    }
    let config_path = copilot_dir.join("config.json");
    if let Err(e) = register_copilot_plugin_in_config(&config_path, &plugin_dir) {
        tracing::warn!(
            target: "copilot_hooks",
            err = %e,
            "failed to register plugin in config.json",
        );
    }

    // Round-7 cleanup: a previous wta wrote files to `_direct/` (which
    // Copilot rejected as an invalid marketplace name). Remove the stale
    // folder so users don't see two copies of the plugin on disk.
    let stale = copilot_dir.join("installed-plugins").join("_direct");
    if stale.is_dir() {
        if let Err(e) = fs::remove_dir_all(&stale) {
            tracing::warn!(
                target: "copilot_hooks",
                err = %e,
                path = %stale.display(),
                "failed to remove stale _direct folder; non-fatal",
            );
        } else {
            tracing::info!(
                target: "copilot_hooks",
                path = %stale.display(),
                "removed stale _direct plugin folder",
            );
        }
    }
}

/// Return the discovered home directory. Mirrors `history_loader::home_dir`
/// so behavior is consistent between the two modules.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Copilot plugin install — separate code path because Copilot CLI ignores
// the top-level `hooks` block and only loads hooks declared by registered
// plugins.
// ---------------------------------------------------------------------------

/// Write the marketplace catalog files (`marketplace.json`) into
/// `installed-plugins/wt-local/.claude-plugin/`. Copilot CLI's plugin
/// manager scans `extraKnownMarketplaces` and reads each
/// `<marketplace>/.claude-plugin/marketplace.json` to discover plugins.
fn write_copilot_marketplace_files(marketplace_dir: &Path) -> std::io::Result<()> {
    let claude_plugin_dir = marketplace_dir.join(".claude-plugin");
    fs::create_dir_all(&claude_plugin_dir)?;

    let marketplace_json = serde_json::to_string_pretty(&copilot_marketplace_json_value())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    write_if_changed(
        &claude_plugin_dir.join("marketplace.json"),
        &marketplace_json,
    )?;
    Ok(())
}

/// Build the `marketplace.json` document Copilot's plugin manager reads.
/// The `source: "./<plugin-folder>"` is resolved relative to the
/// marketplace folder when Copilot loads it.
fn copilot_marketplace_json_value() -> Value {
    json!({
        "name":        COPILOT_MARKETPLACE_NAME,
        "description": "Local marketplace populated by wta",
        "owner":       { "name": "Agentic Terminal" },
        "plugins": [
            {
                "name":        COPILOT_PLUGIN_NAME,
                "description": COPILOT_PLUGIN_DESCRIPTION,
                "version":     COPILOT_PLUGIN_VERSION,
                "source":      format!("./{}", COPILOT_PLUGIN_DIR_NAME),
            }
        ],
    })
}

/// Write the plugin files (`.claude-plugin/plugin.json`,
/// `hooks/hooks.json`, `hooks/send-event.ps1`) into the plugin folder.
/// Idempotent: each file is only rewritten when its on-disk content
/// differs from what we'd produce.
///
/// **Manifest path** is `.claude-plugin/plugin.json`, NOT `plugin.json`
/// at the plugin root. Copilot's loader silently ignores a root-level
/// manifest (matching the `superpowers` plugin convention). Earlier wta
/// builds wrote to the root and the plugin never loaded.
fn write_copilot_plugin_files(plugin_dir: &Path) -> std::io::Result<()> {
    let claude_plugin_subdir = plugin_dir.join(".claude-plugin");
    let hooks_subdir = plugin_dir.join("hooks");
    fs::create_dir_all(&claude_plugin_subdir)?;
    fs::create_dir_all(&hooks_subdir)?;

    let plugin_json = serde_json::to_string_pretty(&copilot_plugin_json_value())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    write_if_changed(&claude_plugin_subdir.join("plugin.json"), &plugin_json)?;
    write_if_changed(&hooks_subdir.join("send-event.ps1"), SEND_EVENT_PS1)?;

    // Generate hooks.json from `HOOK_EVENTS` so the registered events stay
    // in sync with what the rest of this module already manages for Claude.
    // Use `${CLAUDE_PLUGIN_ROOT}` resolution so the plugin keeps working if
    // the user moves their `.copilot` dir (Copilot CLI substitutes the
    // plugin's own folder for that variable).
    let hooks_json = serde_json::to_string_pretty(&copilot_hooks_json_value())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    write_if_changed(&hooks_subdir.join("hooks.json"), &hooks_json)?;

    // Pre-round-7 wta wrote a root-level `plugin.json` that Copilot
    // ignored. Remove it so users don't see two copies of the manifest.
    let stale_root_manifest = plugin_dir.join("plugin.json");
    if stale_root_manifest.is_file() {
        if let Err(e) = fs::remove_file(&stale_root_manifest) {
            tracing::warn!(
                target: "copilot_hooks",
                err = %e,
                "failed to remove stale root plugin.json; non-fatal",
            );
        }
    }

    Ok(())
}

/// Build the `plugin.json` manifest written into
/// `<plugin-root>/.claude-plugin/plugin.json`.
///
/// Deliberately omits a `hooks` field — Copilot's loader auto-discovers
/// `<plugin-root>/hooks/hooks.json` by convention (matches the
/// `superpowers` plugin), and the embedded reference manifest's `"hooks":
/// "hooks/hooks.json"` field has caused at least one reported parse warning
/// in the wild.
fn copilot_plugin_json_value() -> Value {
    json!({
        "name":        COPILOT_PLUGIN_NAME,
        "description": COPILOT_PLUGIN_DESCRIPTION,
        "version":     COPILOT_PLUGIN_VERSION,
        "author":      { "name": "Agentic Terminal" },
        "license":     "MIT",
        "keywords":    ["windows-terminal", "agent-hooks", "wta"],
    })
}

/// Build the `hooks.json` document Copilot's plugin loader will read.
/// Mirrors the on-disk format `wta/agent-hooks-plugin/hooks/hooks.json`
/// uses but generated programmatically from `HOOK_EVENTS` (so we don't ship
/// stale event names).
fn copilot_hooks_json_value() -> Value {
    let mut hooks_map = serde_json::Map::new();
    for (event_name, event_id) in HOOK_EVENTS {
        hooks_map.insert(
            (*event_name).to_string(),
            json!([{
                "matcher": ".*",
                "hooks": [{
                    "type": "command",
                    "command": format!(
                        "powershell -ExecutionPolicy Bypass -File \"${{CLAUDE_PLUGIN_ROOT}}/hooks/send-event.ps1\" -CliSource copilot {}",
                        event_id,
                    ),
                }]
            }]),
        );
    }
    json!({ "hooks": Value::Object(hooks_map) })
}

/// Register the deployed plugin in `~/.copilot/settings.json`. Manages
/// the user-facing parts: the marketplace registration and the enabled
/// flag. The actual `installedPlugins[]` entry lives in `config.json`
/// (managed by `register_copilot_plugin_in_config`) — that's the file
/// Copilot CLI's plugin loader reads when populating
/// `copilot plugin list`.
///
/// Idempotent edits:
///
///   1. Set `extraKnownMarketplaces["wt-local"] = { source: { source:
///      "directory", path: <abs path to marketplace folder> } }`.
///   2. Set `enabledPlugins["wt-agent-hooks@wt-local"] = true`.
///   3. Cleanup of legacy state:
///        * Remove top-level wta-tagged `hooks` entries (round-5 mistake).
///        * Remove any `installedPlugins[]` entries we wrote here in
///          round-6/7 (now relocated to config.json), plus any with
///          `marketplace == "_direct"`.
///        * Remove `enabledPlugins["wt-agent-hooks@_direct"]`.
fn register_copilot_plugin_in_settings(
    settings_path: &Path,
    marketplace_dir: &Path,
) -> std::io::Result<()> {
    let mut settings: Value = match fs::read_to_string(settings_path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed settings.json (leaving untouched): {}", e),
            )
        })?,
        _ => Value::Object(serde_json::Map::new()),
    };

    let marketplace_path = marketplace_dir.to_string_lossy().into_owned();
    let mut changed = false;
    changed |= ensure_extra_known_marketplace(&mut settings, &marketplace_path);
    changed |= ensure_plugin_enabled(&mut settings);
    changed |= remove_wta_tagged_top_level_hooks(&mut settings);
    changed |= remove_legacy_direct_marketplace_entries(&mut settings);
    changed |= remove_wt_agent_hooks_installed_plugin_from_settings(&mut settings);

    if !changed {
        tracing::debug!(target: "copilot_hooks", "settings.json already registers plugin");
        return Ok(());
    }

    let serialized = serde_json::to_string_pretty(&settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(settings_path, serialized)?;
    tracing::info!(
        target: "copilot_hooks",
        path = %settings_path.display(),
        "registered wt-agent-hooks plugin",
    );
    Ok(())
}

/// Register / refresh the `installedPlugins[]` entry in
/// `~/.copilot/config.json`. **This is the file Copilot CLI's plugin
/// loader reads** to populate `copilot plugin list` — round-6/-7 wta
/// wrote to settings.json instead, which Copilot ignored. The CLI marks
/// config.json with `// This file is managed automatically.` but it's
/// the only path that produces a working install without invoking the
/// CLI itself.
///
/// Idempotent edits:
///
///   1. Append a fully-formed entry to `installedPlugins` iff no element
///      with `name == "wt-agent-hooks"` and `marketplace == "wt-local"`
///      already exists; otherwise refresh `cache_path`/`version`/`enabled`.
///   2. Strip legacy `_direct` `installedPlugins[]` entries so we don't
///      leave dead rows lying around after migration.
fn register_copilot_plugin_in_config(
    config_path: &Path,
    plugin_dir: &Path,
) -> std::io::Result<()> {
    let mut config: Value = match fs::read_to_string(config_path) {
        Ok(text) if !text.trim().is_empty() => parse_jsonc(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed config.json (leaving untouched): {}", e),
            )
        })?,
        _ => Value::Object(serde_json::Map::new()),
    };

    let cache_path = plugin_dir.to_string_lossy().into_owned();
    let mut changed = false;
    changed |= upsert_installed_plugin(&mut config, &cache_path);
    changed |= remove_legacy_direct_marketplace_entries(&mut config);

    if !changed {
        tracing::debug!(target: "copilot_hooks", "config.json already registers plugin");
        return Ok(());
    }

    let serialized = serde_json::to_string_pretty(&config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(config_path, serialized)?;
    tracing::info!(
        target: "copilot_hooks",
        path = %config_path.display(),
        "registered wt-agent-hooks plugin in config.json",
    );
    Ok(())
}

/// Strip from `settings.json` any `installedPlugins[]` entries with
/// `name == "wt-agent-hooks"`. Pre-round-7 wta wrote `installedPlugins[]`
/// to settings.json, but Copilot reads it from config.json — leaving the
/// entry behind in settings.json is harmless but creates two sources of
/// truth that drift. Returns `true` if the document changed.
fn remove_wt_agent_hooks_installed_plugin_from_settings(settings: &mut Value) -> bool {
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let arr = match root
        .get_mut("installedPlugins")
        .and_then(|v| v.as_array_mut())
    {
        Some(a) => a,
        None => return false,
    };
    let before = arr.len();
    arr.retain(|e| e.get("name").and_then(|v| v.as_str()) != Some(COPILOT_PLUGIN_NAME));
    arr.len() != before
}

/// Permissive JSON parser used for `config.json`, which Copilot CLI
/// prefixes with `// User settings ...` C-style comments. `serde_json`
/// rejects those, so we strip line comments before parsing.
///
/// Only handles `//` line comments — the Copilot file doesn't use block
/// comments. We deliberately avoid a JSONC crate dependency for one file.
fn parse_jsonc(text: &str) -> Result<Value, serde_json::Error> {
    let mut cleaned = String::with_capacity(text.len());
    for line in text.lines() {
        // Strip everything from `//` to end of line, but only when `//`
        // is not inside a JSON string literal. Cheap heuristic: scan
        // chars, toggle in_string on unescaped quotes.
        let mut in_string = false;
        let mut prev = '\0';
        let mut comment_start: Option<usize> = None;
        for (i, c) in line.char_indices() {
            if c == '"' && prev != '\\' {
                in_string = !in_string;
            } else if !in_string && c == '/' && prev == '/' {
                comment_start = Some(i - 1);
                break;
            }
            prev = c;
        }
        match comment_start {
            Some(idx) => cleaned.push_str(&line[..idx]),
            None => cleaned.push_str(line),
        }
        cleaned.push('\n');
    }
    serde_json::from_str(&cleaned)
}

/// Add or refresh `extraKnownMarketplaces["wt-local"]` so Copilot CLI's
/// plugin manager scans our marketplace folder. Returns `true` if the
/// document changed.
///
/// Note: Copilot's marketplace `source` discriminator is `"directory"`
/// (not `"local"`) — that's the value `copilot plugin marketplace add
/// <local-path>` writes.
fn ensure_extra_known_marketplace(settings: &mut Value, marketplace_path: &str) -> bool {
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let map = root
        .entry("extraKnownMarketplaces".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let map = match map.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let desired = json!({
        "source": {
            "source": "directory",
            "path":   marketplace_path,
        }
    });

    if map.get(COPILOT_MARKETPLACE_NAME) == Some(&desired) {
        return false;
    }
    map.insert(COPILOT_MARKETPLACE_NAME.to_string(), desired);
    true
}

/// Drop legacy state written by pre-round-7 wta builds:
///   * `installedPlugins[]` entries with `marketplace == "_direct"`.
///   * `enabledPlugins["<plugin>@_direct"]` keys.
/// Returns `true` if the document changed.
fn remove_legacy_direct_marketplace_entries(settings: &mut Value) -> bool {
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let mut changed = false;

    if let Some(arr) = root
        .get_mut("installedPlugins")
        .and_then(|v| v.as_array_mut())
    {
        let before = arr.len();
        arr.retain(|entry| entry.get("marketplace").and_then(|v| v.as_str()) != Some("_direct"));
        if arr.len() != before {
            changed = true;
        }
    }

    if let Some(map) = root.get_mut("enabledPlugins").and_then(|v| v.as_object_mut()) {
        let stale_keys: Vec<String> = map
            .keys()
            .filter(|k| k.ends_with("@_direct"))
            .cloned()
            .collect();
        for k in stale_keys {
            map.remove(&k);
            changed = true;
        }
    }

    changed
}

/// Add or update the entry in `installedPlugins`. Returns `true` if the
/// document changed.
fn upsert_installed_plugin(settings: &mut Value, cache_path: &str) -> bool {
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let arr = root
        .entry("installedPlugins".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let entries = match arr.as_array_mut() {
        Some(a) => a,
        None => return false,
    };

    let existing_idx = entries.iter().position(|e| {
        e.get("name").and_then(|v| v.as_str()) == Some(COPILOT_PLUGIN_NAME)
            && e.get("marketplace").and_then(|v| v.as_str())
                == Some(COPILOT_MARKETPLACE_NAME)
    });

    match existing_idx {
        Some(i) => {
            // Refresh the fields we own; preserve `installed_at` so we
            // don't keep churning the timestamp on every wta launch.
            let entry = entries[i].as_object_mut();
            let entry = match entry {
                Some(o) => o,
                None => return false,
            };
            let mut local_changed = false;
            if entry.get("cache_path").and_then(|v| v.as_str()) != Some(cache_path) {
                entry.insert("cache_path".to_string(), Value::String(cache_path.to_string()));
                local_changed = true;
            }
            if entry.get("version").and_then(|v| v.as_str()) != Some(COPILOT_PLUGIN_VERSION) {
                entry.insert(
                    "version".to_string(),
                    Value::String(COPILOT_PLUGIN_VERSION.to_string()),
                );
                local_changed = true;
            }
            if entry.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
                entry.insert("enabled".to_string(), Value::Bool(true));
                local_changed = true;
            }
            if !entry.contains_key("installed_at") {
                entry.insert(
                    "installed_at".to_string(),
                    Value::String(iso_8601_utc_now()),
                );
                local_changed = true;
            }
            local_changed
        }
        None => {
            entries.push(json!({
                "name":         COPILOT_PLUGIN_NAME,
                "marketplace":  COPILOT_MARKETPLACE_NAME,
                "version":      COPILOT_PLUGIN_VERSION,
                "enabled":      true,
                "cache_path":   cache_path,
                "installed_at": iso_8601_utc_now(),
            }));
            true
        }
    }
}

/// Set `enabledPlugins["wt-agent-hooks@wt-local"] = true`. Returns `true`
/// if the document changed.
fn ensure_plugin_enabled(settings: &mut Value) -> bool {
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let map = root
        .entry("enabledPlugins".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let map = match map.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let key = format!("{}@{}", COPILOT_PLUGIN_NAME, COPILOT_MARKETPLACE_NAME);
    if map.get(&key).and_then(|v| v.as_bool()) == Some(true) {
        return false;
    }
    map.insert(key, Value::Bool(true));
    true
}

/// Remove any wta-tagged entries from a top-level `hooks` block in Copilot's
/// settings.json. Older wta builds (pre-round-6) wrongly merged the same
/// block format Claude uses into Copilot's settings, where it has no effect
/// other than visual clutter. We strip those entries here so subsequent
/// `gh copilot --version` / settings inspection produces a clean file.
fn remove_wta_tagged_top_level_hooks(settings: &mut Value) -> bool {
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };
    let hooks = match root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        Some(o) => o,
        None => return false,
    };

    let mut changed = false;
    let event_names: Vec<String> = hooks.keys().cloned().collect();
    for event_name in event_names {
        if let Some(arr) = hooks.get_mut(&event_name).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|entry| !entry_is_wta_tagged(entry));
            if arr.len() != before {
                changed = true;
            }
            if arr.is_empty() {
                hooks.remove(&event_name);
            }
        }
    }
    if changed && hooks.is_empty() {
        root.remove("hooks");
    }
    changed
}

/// Write `contents` to `path` only when the on-disk content differs. Skips
/// the write when unchanged so repeated startups don't churn mtimes.
fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != contents,
        Err(_) => true,
    };
    if needs_write {
        fs::write(path, contents)?;
        tracing::info!(
            target: "copilot_hooks",
            path = %path.display(),
            "wrote plugin file",
        );
    }
    Ok(())
}

/// ISO 8601 UTC timestamp with second precision, e.g. `2026-05-06T15:43:04Z`.
/// Computed from `SystemTime::now()` using `std` only (no chrono dependency)
/// via Howard Hinnant's date algorithms.
fn iso_8601_utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d, hh, mm, ss) = civil_from_unix_secs(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, hh, mm, ss)
}

/// Decompose seconds-since-epoch into `(year, month, day, hour, minute,
/// second)` using Howard Hinnant's `civil_from_days` algorithm. Public-
/// domain reference: <http://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_unix_secs(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let secs_of_day = (secs % 86_400) as u32;
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;

    // Shift epoch from 1970-01-01 to 0000-03-01 to simplify month math.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y_rel = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y_rel + 1 } else { y_rel };
    (y, m, d, hh, mm, ss)
}

/// Write (or refresh) the bridge script under
/// `%LOCALAPPDATA%\IntelligentTerminal\hooks\send-event.ps1` and return its
/// absolute path. Creates the parent directory if needed. Skips the write
/// when the on-disk content already matches the embedded copy, so repeated
/// startups don't churn the file's mtime.
fn write_bridge_script() -> std::io::Result<PathBuf> {
    let root = crate::runtime_paths::intelligent_terminal_root().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "LOCALAPPDATA / APPDATA not set; cannot resolve hooks dir",
        )
    })?;
    let hooks_dir = root.join("hooks");
    fs::create_dir_all(&hooks_dir)?;
    let path = hooks_dir.join("send-event.ps1");

    let needs_write = match fs::read_to_string(&path) {
        Ok(existing) => existing != SEND_EVENT_PS1,
        Err(_) => true,
    };
    if needs_write {
        fs::write(&path, SEND_EVENT_PS1)?;
        tracing::info!(
            target: "claude_hooks",
            path = %path.display(),
            "wrote bridge script",
        );
    }
    Ok(path)
}

/// Idempotently merge our hooks block into the CLI's settings.json.
///
/// Preserves any existing user-defined hooks: for every event we manage,
/// we append our entry to the event's array if no array element already
/// contains a `command` referencing our tag string `WTA_TAG` — otherwise
/// we update that entry's `command` to point at the current script path
/// (handles the case where the path moves between wta versions).
fn ensure_hooks_in_settings(
    settings_path: &Path,
    script_path: &Path,
    log_target: &str,
) -> std::io::Result<()> {
    let mut settings: Value = match fs::read_to_string(settings_path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("malformed settings.json (leaving untouched): {}", e),
            )
        })?,
        _ => Value::Object(serde_json::Map::new()),
    };

    let changed = merge_wta_hooks(&mut settings, script_path);
    if !changed {
        tracing::debug!(target: "claude_hooks", cli = log_target, "settings.json already up to date");
        return Ok(());
    }

    // Pretty-print so users editing settings.json by hand see a familiar
    // 2-space indent rather than a single line.
    let serialized = serde_json::to_string_pretty(&settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(settings_path, serialized)?;
    tracing::info!(
        target: "claude_hooks",
        cli = log_target,
        path = %settings_path.display(),
        "merged hooks block",
    );
    Ok(())
}

/// In-place merge. Returns `true` iff the value actually changed.
fn merge_wta_hooks(settings: &mut Value, script_path: &Path) -> bool {
    let script_command = build_command(script_path);

    // Ensure top-level is an object. If the user's settings.json has a
    // non-object root (extremely unlikely — Claude won't accept it),
    // we leave it alone rather than clobber.
    let root = match settings.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let hooks_obj = match hooks.as_object_mut() {
        Some(o) => o,
        None => return false,
    };

    let mut changed = false;

    // First, remove any wta-tagged entries under event names we no longer
    // emit. This catches users upgrading from older wta builds that wrote
    // an `ErrorOccurred` block — Claude rejects that name and surfaces a
    // "Quick safety check" warning at startup until the entry is gone.
    let current_events: std::collections::HashSet<&str> =
        HOOK_EVENTS.iter().map(|(name, _)| *name).collect();
    let stale_event_names: Vec<String> = hooks_obj
        .keys()
        .filter(|k| !current_events.contains(k.as_str()))
        .cloned()
        .collect();
    for event_name in stale_event_names {
        if let Some(arr) = hooks_obj.get_mut(&event_name).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|entry| !entry_is_wta_tagged(entry));
            if arr.len() != before {
                changed = true;
            }
            if arr.is_empty() {
                hooks_obj.remove(&event_name);
            }
        }
    }

    for (event_name, event_id) in HOOK_EVENTS {
        let arr = hooks_obj
            .entry(event_name.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries = match arr.as_array_mut() {
            Some(a) => a,
            None => continue,
        };

        // Look for an existing wta-tagged entry. We tag by checking whether
        // any nested `command` string contains both WTA_TAG and the event id.
        let existing_idx = entries.iter().position(|entry| {
            entry_contains_wta_command(entry, event_id)
        });

        let desired_entry = json!({
            "matcher": ".*",
            "hooks": [{
                "type":    "command",
                "command": format!("{} {}", script_command, event_id),
            }]
        });

        match existing_idx {
            Some(i) => {
                if entries[i] != desired_entry {
                    entries[i] = desired_entry;
                    changed = true;
                }
            }
            None => {
                entries.push(desired_entry);
                changed = true;
            }
        }
    }

    changed
}

/// True iff the entry was inserted by us (any nested `command` string
/// references our bridge script or carries the WTA_TAG marker). Used when
/// pruning hooks attached to event names that no longer exist in
/// `HOOK_EVENTS`.
fn entry_is_wta_tagged(entry: &Value) -> bool {
    let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    for h in hooks {
        let Some(cmd) = h.get("command").and_then(|c| c.as_str()) else { continue; };
        if cmd.contains(WTA_TAG) || cmd.contains("send-event.ps1") {
            return true;
        }
    }
    false
}

fn entry_contains_wta_command(entry: &Value, event_id: &str) -> bool {
    // Walk the entry's nested `hooks[].command` strings; tagged entries
    // contain both our WTA_TAG marker (path component) and the event id.
    let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    for h in hooks {
        let Some(cmd) = h.get("command").and_then(|c| c.as_str()) else {
            continue;
        };
        if cmd.contains(WTA_TAG) || (cmd.contains("send-event.ps1") && cmd.contains(event_id)) {
            return true;
        }
    }
    false
}

/// Build the PowerShell invocation Claude should run for each hook event.
/// Quoted to survive paths containing spaces. The trailing `-CliSource
/// claude` argument is what tells the bridge script that this invocation
/// came from Claude — without it the script falls through to env-var
/// heuristics and (because Claude registers as top-level hooks, not as
/// a plugin) mis-tags every event as "copilot".
fn build_command(script_path: &Path) -> String {
    format!(
        "powershell -ExecutionPolicy Bypass -File \"{}\" -CliSource claude",
        script_path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_settings_path(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wta-claude-hooks-{}-{}-{}.json",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        p
    }

    fn fake_script() -> PathBuf {
        PathBuf::from("C:\\Users\\me\\AppData\\Local\\IntelligentTerminal\\hooks\\send-event.ps1")
    }

    #[test]
    fn merges_into_empty_settings() {
        let mut v = Value::Object(serde_json::Map::new());
        let changed = merge_wta_hooks(&mut v, &fake_script());
        assert!(changed);
        let hooks = v.get("hooks").and_then(|h| h.as_object()).unwrap();
        for (event, _) in HOOK_EVENTS {
            assert!(hooks.contains_key(*event), "missing event {}", event);
        }
        let pre_tool_use = hooks.get("PreToolUse").and_then(|h| h.as_array()).unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        let cmd = pre_tool_use[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("send-event.ps1"));
        assert!(cmd.contains("agent.tool.starting"));
        // Regression: every Claude hook command MUST carry `-CliSource claude`.
        // Without it, send-event.ps1's env-var heuristics fall through to the
        // legacy default ("copilot") and Claude sessions show up in F2 with
        // the wrong CLI label/icon (e.g. "bb318083-copilot-yuazha").
        assert!(cmd.contains("-CliSource claude"),
            "expected `-CliSource claude` in Claude hook command, got: {}", cmd);
    }

    #[test]
    fn second_merge_is_noop() {
        let mut v = Value::Object(serde_json::Map::new());
        assert!(merge_wta_hooks(&mut v, &fake_script()));
        // Second call should report nothing changed.
        assert!(!merge_wta_hooks(&mut v, &fake_script()));
    }

    #[test]
    fn preserves_unrelated_top_level_keys() {
        let mut v = json!({
            "autoUpdatesChannel": "latest",
            "model": "claude-sonnet-4.5",
        });
        merge_wta_hooks(&mut v, &fake_script());
        assert_eq!(v["autoUpdatesChannel"], "latest");
        assert_eq!(v["model"], "claude-sonnet-4.5");
        assert!(v.get("hooks").is_some());
    }

    #[test]
    fn appends_alongside_user_defined_hooks() {
        let mut v = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-script.sh" }] }
                ]
            }
        });
        let changed = merge_wta_hooks(&mut v, &fake_script());
        assert!(changed);
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        // Original entry must still be present.
        assert!(pre.iter().any(|e| {
            e["hooks"][0]["command"].as_str() == Some("user-script.sh")
        }), "user-defined entry was lost");
        // wta entry must be added.
        assert!(pre.iter().any(|e| {
            e["hooks"][0]["command"].as_str().unwrap_or("").contains("send-event.ps1")
        }), "wta entry missing");
    }

    #[test]
    fn updates_command_path_when_script_moves() {
        let mut v = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": ".*", "hooks": [{
                        "type": "command",
                        "command": "powershell -File \"C:\\old\\send-event.ps1\" agent.tool.starting"
                    }] }
                ]
            }
        });
        let new_path = PathBuf::from("C:\\new\\send-event.ps1");
        let changed = merge_wta_hooks(&mut v, &new_path);
        assert!(changed);
        let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str().unwrap();
        assert!(cmd.contains("C:\\new\\send-event.ps1"), "got {}", cmd);
        // Existing array length stays at 1 (replaced, not appended).
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn prunes_wta_hooks_under_unrecognized_event_names() {
        // Migration test: an older wta wrote `ErrorOccurred` (which Claude
        // does not recognize and therefore rejects with a "Quick safety
        // check" warning). The next ensure_installed run must remove that
        // stale entry. User-defined hooks under the same unknown event
        // name must NOT be touched.
        let mut v = json!({
            "hooks": {
                "ErrorOccurred": [
                    {
                        "matcher": ".*",
                        "hooks": [{
                            "type": "command",
                            "command": "powershell -File \"C:\\old\\send-event.ps1\" agent.error"
                        }]
                    },
                    {
                        "matcher": "X",
                        "hooks": [{ "type": "command", "command": "user-handler.sh" }]
                    }
                ],
                "MysteryEventOnlyUser": [
                    {
                        "matcher": "X",
                        "hooks": [{ "type": "command", "command": "user-handler.sh" }]
                    }
                ]
            }
        });
        let changed = merge_wta_hooks(&mut v, &fake_script());
        assert!(changed);

        // wta-tagged ErrorOccurred entry must be gone, but the user's
        // entry must remain.
        let err = v["hooks"]["ErrorOccurred"].as_array().unwrap();
        assert_eq!(err.len(), 1, "wta entry should be pruned");
        assert_eq!(err[0]["hooks"][0]["command"].as_str(), Some("user-handler.sh"));

        // An unrelated event with only user entries is fully preserved.
        let myst = v["hooks"]["MysteryEventOnlyUser"].as_array().unwrap();
        assert_eq!(myst.len(), 1);
        assert_eq!(myst[0]["hooks"][0]["command"].as_str(), Some("user-handler.sh"));
    }

    #[test]
    fn prunes_empty_unrecognized_event_array_after_cleanup() {
        // If every entry under an unrecognized event was wta-tagged,
        // the now-empty array should disappear entirely so Claude doesn't
        // also complain about an empty hook list.
        let mut v = json!({
            "hooks": {
                "ErrorOccurred": [
                    {
                        "matcher": ".*",
                        "hooks": [{
                            "type": "command",
                            "command": "powershell -File \"C:\\old\\send-event.ps1\" agent.error"
                        }]
                    }
                ]
            }
        });
        merge_wta_hooks(&mut v, &fake_script());
        assert!(v["hooks"].get("ErrorOccurred").is_none(),
            "empty wta-only event should be removed entirely");
    }

    #[test]
    fn ensure_hooks_in_settings_creates_file_when_missing() {
        let path = tmp_settings_path("missing");
        // Note: pass a path that doesn't exist yet.
        ensure_hooks_in_settings(&path, &fake_script(), "test").unwrap();
        let body = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v.get("hooks").is_some());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ensure_hooks_in_settings_preserves_existing_then_idempotent() {
        let path = tmp_settings_path("preserve");
        fs::write(&path, r#"{"autoUpdatesChannel":"latest"}"#).unwrap();

        ensure_hooks_in_settings(&path, &fake_script(), "test").unwrap();
        let v1: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v1["autoUpdatesChannel"], "latest");
        assert!(v1.get("hooks").is_some());

        // Capture mtime, run again, expect no rewrite.
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        ensure_hooks_in_settings(&path, &fake_script(), "test").unwrap();
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "second run must be a no-op");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ensure_hooks_in_settings_skips_malformed_json() {
        let path = tmp_settings_path("malformed");
        fs::write(&path, "{ this is not json").unwrap();
        // Should error rather than overwrite, so the user's broken file is
        // preserved for them to fix manually.
        let res = ensure_hooks_in_settings(&path, &fake_script(), "test");
        assert!(res.is_err());
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("not json"), "malformed file must not be clobbered");
        let _ = fs::remove_file(&path);
    }

    // ---- Copilot plugin install ------------------------------------------

    fn fake_plugin_dir() -> PathBuf {
        PathBuf::from(
            "C:\\Users\\me\\.copilot\\installed-plugins\\wt-local\\wt-agent-hooks",
        )
    }

    fn fake_marketplace_dir() -> PathBuf {
        PathBuf::from("C:\\Users\\me\\.copilot\\installed-plugins\\wt-local")
    }

    #[test]
    fn copilot_hooks_json_uses_plugin_root_variable() {
        // The generated hooks.json must reference ${CLAUDE_PLUGIN_ROOT} so
        // Copilot's plugin loader resolves it to the deployed folder. Using
        // an absolute path here would defeat the point of registering as a
        // plugin (Copilot would just bind a stale path).
        let v = copilot_hooks_json_value();
        let pre = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str().unwrap();
        assert!(pre.contains("${CLAUDE_PLUGIN_ROOT}/hooks/send-event.ps1"),
            "got {}", pre);
        assert!(pre.contains("agent.tool.starting"));
        // Regression: every Copilot hook command MUST carry `-CliSource copilot`
        // so the bridge script doesn't depend on env-var heuristics. Claude
        // and Copilot share plugin shape, so env vars alone aren't sufficient
        // to disambiguate.
        assert!(pre.contains("-CliSource copilot"),
            "expected `-CliSource copilot` in Copilot hook command, got: {}", pre);
        // Every event we manage must be present.
        let hooks = v["hooks"].as_object().unwrap();
        for (event, _) in HOOK_EVENTS {
            assert!(hooks.contains_key(*event), "missing event {}", event);
        }
    }

    #[test]
    fn copilot_register_creates_entry_when_missing() {
        let mut v = json!({
            "model": "gpt-5",
            "installedPlugins": [],
            "enabledPlugins": {}
        });
        let cache_path = fake_plugin_dir().to_string_lossy().into_owned();
        let inst_changed = upsert_installed_plugin(&mut v, &cache_path);
        let en_changed = ensure_plugin_enabled(&mut v);
        assert!(inst_changed && en_changed);

        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let entry = &arr[0];
        assert_eq!(entry["name"], "wt-agent-hooks");
        assert_eq!(entry["marketplace"], "wt-local");
        assert_eq!(entry["enabled"], true);
        assert_eq!(entry["version"], "0.1.0");
        assert_eq!(entry["cache_path"].as_str(), Some(cache_path.as_str()));
        assert!(entry["installed_at"].is_string(), "installed_at must be set");

        // Round-trip through ISO-ish format check.
        let ts = entry["installed_at"].as_str().unwrap();
        assert!(ts.ends_with('Z'), "got {}", ts);
        assert!(ts.contains('T'), "got {}", ts);

        assert_eq!(v["enabledPlugins"]["wt-agent-hooks@wt-local"], true);
        // Unrelated key preserved.
        assert_eq!(v["model"], "gpt-5");
    }

    #[test]
    fn copilot_register_is_idempotent() {
        let mut v = Value::Object(serde_json::Map::new());
        let cache_path = fake_plugin_dir().to_string_lossy().into_owned();

        assert!(upsert_installed_plugin(&mut v, &cache_path));
        assert!(ensure_plugin_enabled(&mut v));

        // Capture installed_at — it must NOT change on a second run.
        let ts1 = v["installedPlugins"][0]["installed_at"].as_str().unwrap().to_string();

        assert!(!upsert_installed_plugin(&mut v, &cache_path),
            "second upsert should report no change");
        assert!(!ensure_plugin_enabled(&mut v),
            "second ensure_enabled should report no change");

        let ts2 = v["installedPlugins"][0]["installed_at"].as_str().unwrap().to_string();
        assert_eq!(ts1, ts2, "installed_at must be preserved across runs");

        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "plugin must not be duplicated");
    }

    #[test]
    fn copilot_register_refreshes_cache_path_on_drift() {
        // Simulate a wta version that wrote an old absolute path; the next
        // run from a different install location must refresh `cache_path`
        // without re-creating the entry.
        let mut v = json!({
            "installedPlugins": [{
                "name":         "wt-agent-hooks",
                "marketplace":  "wt-local",
                "version":      "0.1.0",
                "enabled":      true,
                "cache_path":   "C:\\old\\path\\agent-hooks-plugin",
                "installed_at": "2026-01-01T00:00:00Z"
            }]
        });
        let new_path = "C:\\new\\path\\agent-hooks-plugin";
        let changed = upsert_installed_plugin(&mut v, new_path);
        assert!(changed);

        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "must update in place, not duplicate");
        assert_eq!(arr[0]["cache_path"].as_str(), Some(new_path));
        // Original installed_at preserved.
        assert_eq!(arr[0]["installed_at"], "2026-01-01T00:00:00Z");
    }

    #[test]
    fn copilot_register_preserves_other_marketplaces() {
        let mut v = json!({
            "installedPlugins": [{
                "name":         "superpowers",
                "marketplace":  "superpowers-marketplace",
                "version":      "5.1.0",
                "enabled":      true,
                "cache_path":   "C:\\unrelated",
                "installed_at": "2026-05-05T13:02:25.428Z"
            }],
            "enabledPlugins": { "superpowers@superpowers-marketplace": true }
        });
        let cache_path = fake_plugin_dir().to_string_lossy().into_owned();

        upsert_installed_plugin(&mut v, &cache_path);
        ensure_plugin_enabled(&mut v);

        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "existing plugin must be preserved");
        // Original superpowers entry untouched.
        let superpowers = arr.iter().find(|e| {
            e["name"].as_str() == Some("superpowers")
        }).expect("superpowers entry missing");
        assert_eq!(superpowers["cache_path"], "C:\\unrelated");

        let enabled = v["enabledPlugins"].as_object().unwrap();
        assert_eq!(enabled["superpowers@superpowers-marketplace"], true);
        assert_eq!(enabled["wt-agent-hooks@wt-local"], true);
    }

    #[test]
    fn remove_wta_top_level_hooks_strips_round5_leftover() {
        // Simulate a settings.json post-round-5 that has the wta-tagged
        // top-level hooks block (which Copilot CLI ignores). Round-6 must
        // strip it as cleanup.
        let mut v = json!({
            "model": "gpt-5",
            "hooks": {
                "PreToolUse": [{
                    "matcher": ".*",
                    "hooks": [{
                        "type": "command",
                        "command": "powershell -File \"C:\\path\\to\\send-event.ps1\" agent.tool.starting"
                    }]
                }],
                "Stop": [{
                    "matcher": ".*",
                    "hooks": [{
                        "type": "command",
                        "command": "powershell -File \"C:\\path\\to\\send-event.ps1\" agent.stop"
                    }]
                }]
            }
        });
        let changed = remove_wta_tagged_top_level_hooks(&mut v);
        assert!(changed);
        // hooks block should be entirely gone (it was wta-only).
        assert!(v.get("hooks").is_none(),
            "empty hooks block should be removed: {}", v);
        // Unrelated keys preserved.
        assert_eq!(v["model"], "gpt-5");
    }

    #[test]
    fn remove_wta_top_level_hooks_preserves_user_entries() {
        let mut v = json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": ".*",
                        "hooks": [{
                            "type": "command",
                            "command": "powershell -File \"C:\\send-event.ps1\" agent.tool.starting"
                        }]
                    },
                    {
                        "matcher": "Bash",
                        "hooks": [{ "type": "command", "command": "user-script.sh" }]
                    }
                ]
            }
        });
        let changed = remove_wta_tagged_top_level_hooks(&mut v);
        assert!(changed);
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"].as_str(), Some("user-script.sh"));
    }

    #[test]
    fn remove_wta_top_level_hooks_noop_on_clean_settings() {
        let mut v = json!({ "model": "gpt-5" });
        assert!(!remove_wta_tagged_top_level_hooks(&mut v));
        assert!(v.get("hooks").is_none());
    }

    #[test]
    fn write_copilot_plugin_files_creates_layout() {
        let root = std::env::temp_dir().join(format!(
            "wta-copilot-plugin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let plugin_dir = root.join("agent-hooks-plugin");

        write_copilot_plugin_files(&plugin_dir).expect("write failed");

        let manifest = plugin_dir.join(".claude-plugin").join("plugin.json");
        assert!(manifest.exists(), "manifest must live in .claude-plugin/");
        assert!(plugin_dir.join("hooks").join("hooks.json").exists());
        assert!(plugin_dir.join("hooks").join("send-event.ps1").exists());

        // No bare plugin.json at the root — Copilot ignores it there.
        assert!(
            !plugin_dir.join("plugin.json").exists(),
            "root-level plugin.json must NOT be present (Copilot ignores it)",
        );

        let manifest_text = fs::read_to_string(&manifest).unwrap();
        assert!(manifest_text.contains("\"name\""));
        assert!(manifest_text.contains("\"wt-agent-hooks\""));
        // Must NOT advertise `hooks` field — Copilot auto-discovers
        // hooks/hooks.json by convention; an explicit field has caused
        // parse warnings in the wild.
        assert!(!manifest_text.contains("\"hooks\""),
            "plugin.json should not declare hooks field");

        let hooks_json = fs::read_to_string(plugin_dir.join("hooks").join("hooks.json")).unwrap();
        assert!(hooks_json.contains("${CLAUDE_PLUGIN_ROOT}/hooks/send-event.ps1"));
        assert!(hooks_json.contains("agent.tool.starting"));

        // Idempotency: second call must not change manifest mtime.
        let mtime_before = fs::metadata(&manifest).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_copilot_plugin_files(&plugin_dir).expect("rewrite failed");
        let mtime_after = fs::metadata(&manifest).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "second run should be a no-op");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_copilot_plugin_files_removes_legacy_root_manifest() {
        // Pre-round-7 wta wrote `<plugin-root>/plugin.json`. The new code
        // path writes `.claude-plugin/plugin.json` instead and must clean
        // up the stale root copy on the next ensure run.
        let root = std::env::temp_dir().join(format!(
            "wta-legacy-root-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let plugin_dir = root.join("agent-hooks-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join("plugin.json"), r#"{"name":"legacy"}"#).unwrap();

        write_copilot_plugin_files(&plugin_dir).expect("write failed");
        assert!(!plugin_dir.join("plugin.json").exists(),
            "legacy root plugin.json must be removed");
        assert!(plugin_dir.join(".claude-plugin").join("plugin.json").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_copilot_marketplace_files_creates_catalog() {
        let root = std::env::temp_dir().join(format!(
            "wta-copilot-marketplace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let marketplace_dir = root.join("wt-local");

        write_copilot_marketplace_files(&marketplace_dir).expect("write failed");

        let catalog = marketplace_dir.join(".claude-plugin").join("marketplace.json");
        assert!(catalog.exists(), "marketplace.json must live in .claude-plugin/");

        let body = fs::read_to_string(&catalog).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["name"], "wt-local");
        let plugins = v["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["name"], "wt-agent-hooks");
        // Source is relative to the marketplace folder (named after the plugin).
        assert_eq!(plugins[0]["source"], "./wt-agent-hooks");

        // Idempotency.
        let mtime_before = fs::metadata(&catalog).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_copilot_marketplace_files(&marketplace_dir).expect("rewrite failed");
        let mtime_after = fs::metadata(&catalog).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_extra_known_marketplace_writes_directory_source() {
        let mut v = Value::Object(serde_json::Map::new());
        let path = "C:\\Users\\me\\.copilot\\installed-plugins\\wt-local";
        assert!(ensure_extra_known_marketplace(&mut v, path));
        let entry = &v["extraKnownMarketplaces"]["wt-local"];
        assert_eq!(entry["source"]["source"], "directory");
        assert_eq!(entry["source"]["path"], path);

        // Idempotent.
        assert!(!ensure_extra_known_marketplace(&mut v, path));
    }

    #[test]
    fn ensure_extra_known_marketplace_refreshes_drifted_path() {
        // wta moved on disk — the path under extraKnownMarketplaces must
        // be refreshed to track. Pre-existing other marketplaces are
        // preserved.
        let mut v = json!({
            "extraKnownMarketplaces": {
                "superpowers-marketplace": {
                    "source": { "source": "github", "owner": "obra", "repo": "superpowers-marketplace" }
                },
                "wt-local": {
                    "source": { "source": "directory", "path": "C:\\OLD\\wt-local" }
                }
            }
        });
        let new_path = "C:\\NEW\\wt-local";
        assert!(ensure_extra_known_marketplace(&mut v, new_path));
        assert_eq!(
            v["extraKnownMarketplaces"]["wt-local"]["source"]["path"],
            new_path,
        );
        // Other marketplace preserved.
        assert_eq!(
            v["extraKnownMarketplaces"]["superpowers-marketplace"]["source"]["source"],
            "github",
        );
    }

    #[test]
    fn remove_legacy_direct_marketplace_entries_cleans_round6_state() {
        // Simulate a settings.json that round-6 wta produced: installed
        // and enabled under marketplace "_direct" (which Copilot rejected).
        let mut v = json!({
            "installedPlugins": [
                {
                    "name": "superpowers",
                    "marketplace": "superpowers-marketplace",
                    "enabled": true
                },
                {
                    "name": "wt-agent-hooks",
                    "marketplace": "_direct",
                    "enabled": true,
                    "cache_path": "C:\\Users\\me\\.copilot\\installed-plugins\\_direct\\agent-hooks-plugin"
                }
            ],
            "enabledPlugins": {
                "superpowers@superpowers-marketplace": true,
                "wt-agent-hooks@_direct": true
            }
        });
        assert!(remove_legacy_direct_marketplace_entries(&mut v));

        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "superpowers");

        let enabled = v["enabledPlugins"].as_object().unwrap();
        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains_key("superpowers@superpowers-marketplace"));
        assert!(!enabled.contains_key("wt-agent-hooks@_direct"));

        // Idempotent.
        assert!(!remove_legacy_direct_marketplace_entries(&mut v));
    }

    #[test]
    fn register_copilot_full_flow_creates_settings_when_missing() {
        let path = tmp_settings_path("copilot-fresh");
        let marketplace_dir = fake_marketplace_dir();
        register_copilot_plugin_in_settings(&path, &marketplace_dir).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["enabledPlugins"]["wt-agent-hooks@wt-local"], true);

        // settings.json should NOT manage installedPlugins[] — that lives in
        // config.json. The array, if present, must not contain our entry.
        if let Some(arr) = v["installedPlugins"].as_array() {
            assert!(
                !arr.iter().any(|e| e["name"].as_str() == Some("wt-agent-hooks")),
                "settings.json must not own wt-agent-hooks installedPlugins entry",
            );
        }

        // Marketplace registered with directory source.
        let mp = &v["extraKnownMarketplaces"]["wt-local"];
        assert_eq!(mp["source"]["source"], "directory");
        assert_eq!(
            mp["source"]["path"].as_str(),
            Some(marketplace_dir.to_string_lossy().as_ref()),
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn register_copilot_idempotent_on_disk() {
        let path = tmp_settings_path("copilot-idem");
        let marketplace_dir = fake_marketplace_dir();
        register_copilot_plugin_in_settings(&path, &marketplace_dir).unwrap();
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        register_copilot_plugin_in_settings(&path, &marketplace_dir).unwrap();
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "second run must be a no-op");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn register_copilot_in_settings_strips_legacy_installed_plugins_entry() {
        // Pre-round-7 wta wrote installedPlugins[] to settings.json. The new
        // code path moves that to config.json and must clean up.
        let path = tmp_settings_path("copilot-strip-legacy");
        fs::write(
            &path,
            r#"{
                "installedPlugins": [
                    {
                        "name": "superpowers",
                        "marketplace": "superpowers-marketplace"
                    },
                    {
                        "name": "wt-agent-hooks",
                        "marketplace": "wt-local",
                        "cache_path": "C:\\stale\\path"
                    }
                ]
            }"#,
        ).unwrap();
        let marketplace_dir = fake_marketplace_dir();
        register_copilot_plugin_in_settings(&path, &marketplace_dir).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v["installedPlugins"].as_array().unwrap();
        assert!(
            !arr.iter().any(|e| e["name"].as_str() == Some("wt-agent-hooks")),
            "legacy wt-agent-hooks entry must be stripped from settings.json",
        );
        // Other plugins preserved.
        assert!(arr.iter().any(|e| e["name"].as_str() == Some("superpowers")));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn register_copilot_in_config_creates_entry_when_missing() {
        let path = tmp_settings_path("copilot-config-fresh");
        // config.json is normally absent on a fresh install, or contains
        // only the auto-managed comment header. Either way the function
        // must create the entry.
        let plugin_dir = fake_plugin_dir();
        register_copilot_plugin_in_config(&path, &plugin_dir).unwrap();
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "wt-agent-hooks");
        assert_eq!(arr[0]["marketplace"], "wt-local");
        assert_eq!(
            arr[0]["cache_path"].as_str(),
            Some(plugin_dir.to_string_lossy().as_ref()),
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn register_copilot_in_config_handles_jsonc_comments() {
        // Copilot CLI prefixes config.json with `// User settings ...`
        // line comments. Our parser must strip them rather than fail.
        let path = tmp_settings_path("copilot-config-jsonc");
        fs::write(
            &path,
            "// User settings belong in settings.json.\n\
             // This file is managed automatically.\n\
             {\n  \"installedPlugins\": []\n}\n",
        ).unwrap();
        let plugin_dir = fake_plugin_dir();
        register_copilot_plugin_in_config(&path, &plugin_dir).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        // Parser strips the comment header — that's fine; the resulting
        // file is valid JSON which is what Copilot CLI actually requires.
        let v: Value = serde_json::from_str(&body).unwrap();
        let arr = v["installedPlugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "wt-agent-hooks");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn register_copilot_in_config_is_idempotent() {
        let path = tmp_settings_path("copilot-config-idem");
        let plugin_dir = fake_plugin_dir();
        register_copilot_plugin_in_config(&path, &plugin_dir).unwrap();
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        register_copilot_plugin_in_config(&path, &plugin_dir).unwrap();
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "second run must be a no-op");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn parse_jsonc_strips_line_comments_outside_strings() {
        let text = r#"// header
{
  "k": "value with // inside string",
  // comment
  "n": 1
}
"#;
        let v = parse_jsonc(text).unwrap();
        assert_eq!(v["k"], "value with // inside string");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn ensure_installed_in_full_flow() {
        // End-to-end: run the installer against a fresh temporary home
        // with realistic .claude/ and .copilot/ subdirs. Verify both code
        // paths complete and produce the expected on-disk layout.
        let root = std::env::temp_dir().join(format!(
            "wta-installer-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));

        // Layout pre-existing user state we want preserved.
        let claude_dir = root.join(".claude");
        let copilot_dir = root.join(".copilot");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::create_dir_all(&copilot_dir).unwrap();
        fs::write(
            claude_dir.join("settings.json"),
            r#"{"autoUpdatesChannel":"latest"}"#,
        ).unwrap();
        fs::write(
            copilot_dir.join("settings.json"),
            r#"{"model":"gpt-5","installedPlugins":[],"enabledPlugins":{}}"#,
        ).unwrap();

        ensure_installed_in(&root);

        // --- Claude side: top-level hooks merged, prior keys preserved.
        let cv: Value = serde_json::from_str(
            &fs::read_to_string(claude_dir.join("settings.json")).unwrap(),
        ).unwrap();
        assert_eq!(cv["autoUpdatesChannel"], "latest", "user key clobbered");
        let chooks = cv["hooks"].as_object().expect("Claude must have hooks");
        for (event, _) in HOOK_EVENTS {
            assert!(chooks.contains_key(*event), "Claude missing event {}", event);
        }

        // --- Copilot side: marketplace + plugin folder deployed and registered.
        let marketplace_dir = copilot_dir.join("installed-plugins").join("wt-local");
        let plugin_dir = marketplace_dir.join("wt-agent-hooks");
        assert!(
            marketplace_dir
                .join(".claude-plugin")
                .join("marketplace.json")
                .exists(),
            "marketplace.json missing",
        );
        assert!(plugin_dir.join(".claude-plugin").join("plugin.json").exists());
        assert!(plugin_dir.join("hooks").join("hooks.json").exists());
        assert!(plugin_dir.join("hooks").join("send-event.ps1").exists());

        // settings.json: marketplace + enabled flag, NO installedPlugins entry.
        let pv: Value = serde_json::from_str(
            &fs::read_to_string(copilot_dir.join("settings.json")).unwrap(),
        ).unwrap();
        assert_eq!(pv["model"], "gpt-5", "user key clobbered");
        assert_eq!(pv["enabledPlugins"]["wt-agent-hooks@wt-local"], true);
        if let Some(arr) = pv["installedPlugins"].as_array() {
            assert!(
                !arr.iter().any(|e| e["name"].as_str() == Some("wt-agent-hooks")),
                "settings.json must not own wt-agent-hooks installedPlugins entry",
            );
        }

        // extraKnownMarketplaces must also point at the marketplace folder.
        let mp_path = pv["extraKnownMarketplaces"]["wt-local"]["source"]["path"]
            .as_str()
            .unwrap();
        assert!(mp_path.ends_with("wt-local"), "got {}", mp_path);

        // Top-level `hooks` block must NOT be present in Copilot's
        // settings.json — round-7 must keep it stripped.
        assert!(pv.get("hooks").is_none(),
            "Copilot's settings.json must not have a top-level hooks block");

        // config.json: installedPlugins[] entry pointing at the plugin folder.
        let cfg_path = copilot_dir.join("config.json");
        let cv: Value = serde_json::from_str(&fs::read_to_string(&cfg_path).unwrap()).unwrap();
        let arr = cv["installedPlugins"].as_array().unwrap();
        let entry = arr.iter().find(|e| {
            e["name"].as_str() == Some("wt-agent-hooks")
                && e["marketplace"].as_str() == Some("wt-local")
        }).expect("wt-agent-hooks must be registered in config.json");
        let cache_path = entry["cache_path"].as_str().unwrap();
        assert!(cache_path.contains("wt-local"));
        assert!(cache_path.contains("wt-agent-hooks"));

        // --- Idempotency: second run rewrites nothing (settings.json AND config.json).
        let claude_mtime = fs::metadata(claude_dir.join("settings.json")).unwrap().modified().unwrap();
        let copilot_mtime = fs::metadata(copilot_dir.join("settings.json")).unwrap().modified().unwrap();
        let config_mtime = fs::metadata(&cfg_path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        ensure_installed_in(&root);
        let claude_mtime2 = fs::metadata(claude_dir.join("settings.json")).unwrap().modified().unwrap();
        let copilot_mtime2 = fs::metadata(copilot_dir.join("settings.json")).unwrap().modified().unwrap();
        let config_mtime2 = fs::metadata(&cfg_path).unwrap().modified().unwrap();
        assert_eq!(claude_mtime, claude_mtime2, "Claude rewrite on idempotent run");
        assert_eq!(copilot_mtime, copilot_mtime2, "Copilot settings rewrite on idempotent run");
        assert_eq!(config_mtime, config_mtime2, "Copilot config rewrite on idempotent run");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_installed_in_strips_leftover_top_level_hooks_from_copilot() {
        // Simulate a user whose Copilot settings.json was previously written
        // by round-5 wta (top-level `hooks` block, ineffective). Round-6
        // must register the plugin AND clean up the dead block.
        let root = std::env::temp_dir().join(format!(
            "wta-installer-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let copilot_dir = root.join(".copilot");
        fs::create_dir_all(&copilot_dir).unwrap();
        // Round-5 leftover: top-level hooks block with wta-tagged entries.
        fs::write(
            copilot_dir.join("settings.json"),
            r#"{
                "model": "gpt-5",
                "installedPlugins": [],
                "enabledPlugins": {},
                "hooks": {
                    "PreToolUse": [{
                        "matcher": ".*",
                        "hooks": [{
                            "type": "command",
                            "command": "powershell -ExecutionPolicy Bypass -File \"C:\\Users\\me\\AppData\\Local\\IntelligentTerminal\\hooks\\send-event.ps1\" agent.tool.starting"
                        }]
                    }]
                }
            }"#,
        ).unwrap();

        ensure_installed_in(&root);

        let pv: Value = serde_json::from_str(
            &fs::read_to_string(copilot_dir.join("settings.json")).unwrap(),
        ).unwrap();
        assert!(pv.get("hooks").is_none(),
            "round-7 must strip the leftover top-level hooks block");
        assert_eq!(pv["enabledPlugins"]["wt-agent-hooks@wt-local"], true);
        assert_eq!(pv["model"], "gpt-5");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_installed_in_migrates_round6_direct_marketplace_to_wt_local() {
        // Simulate a settings.json shaped by round-6 wta: plugin installed
        // and enabled under the rejected marketplace name "_direct", with
        // a folder at installed-plugins/_direct/. Round-7 must:
        //   * Add wt-local marketplace + plugin entries.
        //   * Strip the _direct entries from BOTH settings.json AND config.json.
        //   * Delete the legacy _direct folder on disk.
        let root = std::env::temp_dir().join(format!(
            "wta-installer-migrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let copilot_dir = root.join(".copilot");
        fs::create_dir_all(&copilot_dir).unwrap();
        // Pre-existing legacy folder layout.
        let legacy_plugin = copilot_dir
            .join("installed-plugins")
            .join("_direct")
            .join("agent-hooks-plugin");
        fs::create_dir_all(&legacy_plugin).unwrap();
        fs::write(legacy_plugin.join("plugin.json"), r#"{"name":"legacy"}"#).unwrap();
        // Pre-existing legacy settings.json entries (round-6 layout had
        // installedPlugins[] in settings.json instead of config.json).
        fs::write(
            copilot_dir.join("settings.json"),
            r#"{
                "installedPlugins": [{
                    "name": "wt-agent-hooks",
                    "marketplace": "_direct",
                    "enabled": true,
                    "version": "0.1.0",
                    "cache_path": "C:\\old\\path",
                    "installed_at": "2026-01-01T00:00:00Z"
                }],
                "enabledPlugins": { "wt-agent-hooks@_direct": true }
            }"#,
        ).unwrap();

        ensure_installed_in(&root);

        // Legacy folder gone.
        assert!(
            !copilot_dir.join("installed-plugins").join("_direct").exists(),
            "legacy _direct/ folder must be deleted",
        );
        // New folder present (named after plugin name).
        assert!(copilot_dir
            .join("installed-plugins")
            .join("wt-local")
            .join("wt-agent-hooks")
            .join(".claude-plugin")
            .join("plugin.json")
            .exists());

        // settings.json: cleaned (no _direct, no wt-agent-hooks installedPlugins entry).
        let pv: Value = serde_json::from_str(
            &fs::read_to_string(copilot_dir.join("settings.json")).unwrap(),
        ).unwrap();
        if let Some(arr) = pv["installedPlugins"].as_array() {
            assert!(
                !arr.iter().any(|e| e["name"].as_str() == Some("wt-agent-hooks")),
                "settings.json must not own wt-agent-hooks installedPlugins entry",
            );
            assert!(
                !arr.iter().any(|e| e["marketplace"].as_str() == Some("_direct")),
                "_direct entries must be stripped from settings.json",
            );
        }
        assert_eq!(pv["enabledPlugins"]["wt-agent-hooks@wt-local"], true);
        assert!(
            pv["enabledPlugins"]
                .as_object()
                .unwrap()
                .get("wt-agent-hooks@_direct")
                .is_none(),
            "stale @_direct enabled key must be removed",
        );

        // config.json: new wt-local entry with cache_path pointing at the new folder.
        let cv: Value = serde_json::from_str(
            &fs::read_to_string(copilot_dir.join("config.json")).unwrap(),
        ).unwrap();
        let arr = cv["installedPlugins"].as_array().unwrap();
        let entry = arr.iter().find(|e| {
            e["name"].as_str() == Some("wt-agent-hooks")
                && e["marketplace"].as_str() == Some("wt-local")
        }).expect("wt-local entry must exist in config.json");
        let cache_path = entry["cache_path"].as_str().unwrap();
        assert!(cache_path.contains("wt-local"));
        assert!(cache_path.ends_with("wt-agent-hooks"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn iso_8601_format_is_well_formed() {
        let ts = iso_8601_utc_now();
        // "YYYY-MM-DDTHH:MM:SSZ" → 20 chars
        assert_eq!(ts.len(), 20, "got {} ({} chars)", ts, ts.len());
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn civil_from_unix_secs_known_dates() {
        // 1970-01-01T00:00:00Z — epoch.
        assert_eq!(civil_from_unix_secs(0), (1970, 1, 1, 0, 0, 0));

        // 2026-05-06T15:43:04Z. Days from 1970-01-01:
        //   56 years × 365 + 14 leap days (1972..2024 inclusive) = 20454 days
        //   + Jan(31) + Feb(28) + Mar(31) + Apr(30) + 5 = 125 days into 2026
        //   = day 20579.
        let secs = 20_579_u64 * 86_400 + 15 * 3600 + 43 * 60 + 4;
        assert_eq!(civil_from_unix_secs(secs), (2026, 5, 6, 15, 43, 4));

        // Round-trip: today + 1 day must be exactly +86400 seconds, year/
        // month/day must increment monotonically. Cheap regression check
        // for off-by-one errors in the algorithm.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap().as_secs();
        let (y0, m0, d0, _, _, _) = civil_from_unix_secs(now);
        let (y1, m1, d1, _, _, _) = civil_from_unix_secs(now + 86_400);
        // Either day rolls over within month, or month/year rolls.
        let advanced = (y0, m0, d0 + 1) == (y1, m1, d1)
            || (m1 != m0)
            || (y1 != y0);
        assert!(advanced, "+1 day should advance: {:?} → {:?}",
            (y0, m0, d0), (y1, m1, d1));
    }
}
