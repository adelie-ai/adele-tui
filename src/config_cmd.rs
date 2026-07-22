//! Non-interactive `config` subcommand handlers (adele-tui#122).
//!
//! The scriptable twin of the interactive `F5` MCP-servers panel and the
//! connection/config screens: `adele config …` loads, mutates, and saves the
//! shared client-MCP config ([`ClientMcpConfig`], `client-mcp.toml`) without ever
//! standing up a TUI or a daemon connection, so it composes in shell scripts.
//!
//! Every handler here is **pure over an injected config path** — the caller
//! resolves [`default_client_mcp_path`] and passes it in — and writes its
//! human-facing output to an injected [`Write`] sink, so the whole surface is
//! unit-testable against a tempfile and an in-memory buffer with no real daemon,
//! filesystem-global state, or terminal.
//!
//! Scope: this manages the **client-side** MCP config (`client-mcp.toml`) and,
//! via [`config_set`] / [`config_get`], the client-local preferences in
//! `settings.json` (currently the `share-client-context` device-info off-switch,
//! da#549). Daemon-hosted MCP servers are out of this cut (they need a live
//! connection + the typed command channel the `F5` panel uses); [`mcp_list`]
//! says so in its output.
//!
//! [`default_client_mcp_path`]: desktop_assistant_client_common::mcp_host::default_client_mcp_path

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use desktop_assistant_client_common::mcp_host::{ClientMcpConfig, McpServerConfig};

use crate::settings::Settings;

/// The default client surface the `config mcp` subcommands act on when none is
/// given — the TUI's own surface. Mirrors the `"tui"` surface string the
/// interactive client hosts its client-MCP servers under.
pub const DEFAULT_SURFACE: &str = "tui";

/// A compiled-in built-in server, reduced to what [`mcp_list`] renders: its name
/// and advertised tool count. The caller resolves these from
/// `crate::builtins::builtin_servers()` (`name` + `service.tools().len()`) and
/// passes them in, so this module stays free of the feature-gated built-in
/// server deps and is testable with synthetic built-ins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinInfo {
    /// The built-in's name (also its default tool-namespace prefix).
    pub name: String,
    /// The number of tools the built-in advertises (`service.tools().len()`).
    pub tool_count: usize,
}

impl BuiltinInfo {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, tool_count: usize) -> Self {
        Self {
            name: name.into(),
            tool_count,
        }
    }
}

/// `config path`: print the resolved client-MCP config file location.
///
/// This file is shared per machine across every Adele client (each surface
/// selects its own subset), so the path is the same one the interactive TUI,
/// GTK, and KDE clients read.
pub fn config_path(path: &Path, out: &mut impl Write) -> Result<()> {
    writeln!(out, "Client-MCP config: {}", path.display())?;
    writeln!(
        out,
        "  (shared per machine; each client surface selects its own subset)"
    )?;
    Ok(())
}

/// `config show [--section mcp]`: print the effective client-MCP config as TOML.
///
/// `section` restricts the output; only `mcp` (the default) is supported today,
/// and any other value is a clear error rather than an empty print.
pub fn config_show(path: &Path, section: Option<&str>, out: &mut impl Write) -> Result<()> {
    match section {
        None | Some("mcp") => {}
        Some(other) => bail!("unknown config section '{other}'; only 'mcp' is supported"),
    }

    writeln!(out, "Client-MCP config: {}", path.display())?;
    if !path.exists() {
        writeln!(
            out,
            "  (file does not exist — no client-hosted MCP servers configured)"
        )?;
        return Ok(());
    }

    // Load (tolerant of a malformed file, which parses to an empty config with a
    // warning) and re-serialize the *effective* config, so `show` reflects what
    // the clients actually see rather than echoing raw bytes.
    let cfg = ClientMcpConfig::load(path);
    if cfg.list_defined_servers().is_empty() {
        writeln!(out, "  (no client-MCP servers defined)")?;
    }
    let toml = toml::to_string_pretty(&cfg).map_err(|e| anyhow!("serialize config: {e}"))?;
    write!(out, "{toml}")?;
    Ok(())
}

/// `config mcp list`: list the client-MCP servers for `surface` (with their
/// command + whether they're enabled for that surface), then the compiled-in
/// built-ins (with tool counts). A built-in is marked `disabled (config)` when
/// it is explicitly turned off for the surface, else `overridden ...` when a
/// same-named, surface-enabled client-MCP server shadows it, else `active`.
pub fn mcp_list(
    path: &Path,
    builtins: &[BuiltinInfo],
    surface: &str,
    out: &mut impl Write,
) -> Result<()> {
    let cfg = ClientMcpConfig::load(path);
    let defined = cfg.list_defined_servers();
    let enabled_names = cfg.surface_enabled_names(surface);
    // A built-in is overridden when a same-named client-MCP server actually
    // resolves for this surface (defined + enabled + surface-listed) — exactly
    // the shadowing rule `McpHost::start_with` applies.
    let overriding: Vec<&str> = cfg
        .resolved_servers(surface)
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    // Built-ins the user explicitly turned off for this surface (da#538 slice 4).
    // A config-disable is the explicit off-switch, so it takes display precedence
    // over an override — mirroring `server_rows_with_builtins`.
    let disabled_builtins = cfg.surface_disabled_builtins(surface);

    // Column width for the name column, sized to the longest name shown.
    let name_w = defined
        .iter()
        .map(|s| s.name.len())
        .chain(builtins.iter().map(|b| b.name.len()))
        .max()
        .unwrap_or(4)
        .max(4);

    writeln!(out, "Client-hosted MCP servers (surface: {surface})")?;
    if defined.is_empty() {
        writeln!(
            out,
            "  (none defined — add one with `config mcp add-server`)"
        )?;
    } else {
        writeln!(out, "  {:name_w$}  {:<9}  COMMAND", "NAME", "STATUS")?;
        for server in defined {
            let status = if enabled_names.iter().any(|n| n == &server.name) {
                "enabled"
            } else {
                "disabled"
            };
            let target = if server.command.is_empty() {
                "(remote/http)".to_string()
            } else if server.args.is_empty() {
                server.command.clone()
            } else {
                format!("{} {}", server.command, server.args.join(" "))
            };
            writeln!(out, "  {:name_w$}  {status:<9}  {target}", server.name)?;
        }
    }

    writeln!(out)?;
    writeln!(out, "Built-in (in-process) servers")?;
    if builtins.is_empty() {
        writeln!(out, "  (none compiled in)")?;
    } else {
        writeln!(out, "  {:name_w$}  {:<5}  STATUS", "NAME", "TOOLS")?;
        for builtin in builtins {
            let status = if disabled_builtins.iter().any(|n| n == &builtin.name) {
                "disabled (config)".to_string()
            } else if overriding.iter().any(|n| *n == builtin.name) {
                format!("overridden by client-MCP '{}'", builtin.name)
            } else {
                "active".to_string()
            };
            writeln!(
                out,
                "  {:name_w$}  {:<5}  {status}",
                builtin.name, builtin.tool_count
            )?;
        }
    }

    writeln!(out)?;
    writeln!(
        out,
        "Note: daemon-hosted MCP servers are not shown here (client-side config only)."
    )?;
    writeln!(
        out,
        "      Manage those from the interactive `adele` F5 panel."
    )?;
    Ok(())
}

/// `config mcp add-server`: define (or replace) a stdio client-MCP server, and
/// — when `enabled` — turn it on for each of `surfaces`.
#[allow(clippy::too_many_arguments)]
pub fn mcp_add_server(
    path: &Path,
    name: &str,
    command: &str,
    args: &[String],
    namespace: Option<&str>,
    surfaces: &[String],
    enabled: bool,
    out: &mut impl Write,
) -> Result<()> {
    let mut cfg = ClientMcpConfig::load(path);
    let existed = cfg.list_defined_servers().iter().any(|s| s.name == name);

    // The definition is always enabled at the definition level (the surface
    // enable list is the on/off switch the `enable`/`disable` subcommands drive);
    // `--enabled` decides whether it is turned on for the given surface(s) now.
    let server = McpServerConfig {
        name: name.to_string(),
        command: command.to_string(),
        args: args.to_vec(),
        namespace: namespace.map(str::to_string),
        enabled: true,
        env: HashMap::new(),
        env_secrets: HashMap::new(),
        http: None,
        description: None,
    };
    cfg.upsert_server(server);
    if enabled {
        for surface in surfaces {
            cfg.set_surface_enabled(surface, name, true);
        }
    }
    cfg.save(path).map_err(|e| anyhow!(e))?;

    writeln!(
        out,
        "{} client-MCP server '{name}' (command: {command}).",
        if existed { "Updated" } else { "Added" }
    )?;
    if enabled && !surfaces.is_empty() {
        writeln!(out, "Enabled for surface(s): {}.", surfaces.join(", "))?;
    } else {
        writeln!(
            out,
            "Defined but not enabled for any surface; run `adele config mcp enable {name}` to turn it on."
        )?;
    }
    Ok(())
}

/// `config mcp remove-server`: delete a client-MCP server definition (and prune
/// it from every surface). Errors if no server by that name is defined.
pub fn mcp_remove_server(path: &Path, name: &str, out: &mut impl Write) -> Result<()> {
    let mut cfg = ClientMcpConfig::load(path);
    cfg.remove_server(name).map_err(|e| anyhow!(e))?;
    cfg.save(path).map_err(|e| anyhow!(e))?;
    writeln!(out, "Removed client-MCP server '{name}'.")?;
    Ok(())
}

/// `config mcp enable`/`disable`: turn a server or built-in on/off for one
/// surface.
///
/// Precedence mirrors the interactive host: a name that matches a **defined**
/// client-MCP server toggles that server's per-surface enable list, even if a
/// built-in of the same name also exists (the server shadows the built-in). A
/// name that matches only a **built-in** toggles that built-in's per-surface
/// `disabled_builtins` list (`on = false` disables it, `on = true` re-enables
/// it) — da#538 slice 4. A name that matches neither is an error.
///
/// A built-in toggled here takes effect on the next client launch (the running
/// in-process host is not restarted), matching the F5 panel's behavior.
pub fn mcp_set_enabled(
    path: &Path,
    name: &str,
    surface: &str,
    on: bool,
    builtins: &[BuiltinInfo],
    out: &mut impl Write,
) -> Result<()> {
    let mut cfg = ClientMcpConfig::load(path);
    let is_defined = cfg.list_defined_servers().iter().any(|s| s.name == name);

    if is_defined {
        cfg.set_surface_enabled(surface, name, on);
        cfg.save(path).map_err(|e| anyhow!(e))?;
        writeln!(
            out,
            "{} client-MCP server '{name}' for surface '{surface}'.",
            if on { "Enabled" } else { "Disabled" }
        )?;
    } else if builtins.iter().any(|b| b.name == name) {
        // A name that matches only a built-in: record its per-surface off state.
        // `on = false` disables (adds to `disabled_builtins`); `on = true`
        // re-enables (removes it). Both are idempotent and per-surface.
        cfg.set_builtin_disabled(surface, name, !on);
        cfg.save(path).map_err(|e| anyhow!(e))?;
        writeln!(
            out,
            "{} built-in '{name}' for surface '{surface}' (applies on next launch).",
            if on { "Enabled" } else { "Disabled" }
        )?;
    } else {
        bail!("no such client-MCP server: '{name}'");
    }
    Ok(())
}

/// The `config set`/`config get` key for the "Share device info with the
/// assistant" preference ([`Settings::share_client_context`], da#549).
pub const SHARE_CLIENT_CONTEXT_KEY: &str = "share-client-context";

/// `config set <KEY> <VALUE>`: persist a client-local preference to the
/// `settings.json` at `path`.
///
/// Only [`SHARE_CLIENT_CONTEXT_KEY`] is recognized today; any other key is a
/// clear error naming the known key rather than a silent no-op. The value is
/// parsed leniently (`on`/`off`, `true`/`false`, `yes`/`no`, `1`/`0`, any case)
/// but strictly rejects anything else -- an unparseable value never touches the
/// file. Other settings in the file are preserved (load, mutate one field,
/// save).
pub fn config_set(path: &Path, key: &str, value: &str, out: &mut impl Write) -> Result<()> {
    match key {
        SHARE_CLIENT_CONTEXT_KEY => {
            // Parse BEFORE loading/saving so a bad value leaves the file untouched.
            let on = parse_on_off(value)?;
            let mut settings = Settings::load_from(path);
            settings.share_client_context = on;
            settings
                .save_to(path)
                .map_err(|e| anyhow!("saving settings to {}: {e}", path.display()))?;
            writeln!(out, "{SHARE_CLIENT_CONTEXT_KEY} = {}", on_off(on))?;
            Ok(())
        }
        other => bail!("unknown setting '{other}'; known keys: {SHARE_CLIENT_CONTEXT_KEY}"),
    }
}

/// `config get <KEY>`: print a client-local preference's current value from the
/// `settings.json` at `path` (an absent file reports the default). Unknown keys
/// error the same way [`config_set`] does.
pub fn config_get(path: &Path, key: &str, out: &mut impl Write) -> Result<()> {
    match key {
        SHARE_CLIENT_CONTEXT_KEY => {
            let settings = Settings::load_from(path);
            writeln!(
                out,
                "{SHARE_CLIENT_CONTEXT_KEY} = {}",
                on_off(settings.share_client_context)
            )?;
            Ok(())
        }
        other => bail!("unknown setting '{other}'; known keys: {SHARE_CLIENT_CONTEXT_KEY}"),
    }
}

/// Render a boolean preference as the `on`/`off` token the CLI reads and writes.
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// Parse a human on/off value. Lenient in encoding (accepts `on`/`off`,
/// `true`/`false`, `yes`/`no`, `1`/`0`, any case, surrounding whitespace),
/// strict in value (anything else is a clear error, never a silent default).
fn parse_on_off(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        other => bail!("invalid value '{other}'; use on/off (also true/false, yes/no, 1/0)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_client_common::mcp_host::ClientMcpConfig;
    use tempfile::tempdir;

    /// A fresh tempdir + the config path inside it (the file itself does not yet
    /// exist, exercising the load-absent-as-default path).
    fn temp_cfg() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("client-mcp.toml");
        (dir, path)
    }

    fn out_string(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).expect("handler ok");
        String::from_utf8(buf).expect("utf8 output")
    }

    #[test]
    fn add_server_then_list_includes_it() {
        let (_dir, path) = temp_cfg();

        mcp_add_server(
            &path,
            "notes",
            "notes-mcp",
            &["serve".to_string()],
            Some("nt"),
            &["tui".to_string()],
            true,
            &mut Vec::new(),
        )
        .expect("add");

        // The definition was written and reloads.
        let cfg = ClientMcpConfig::load(&path);
        let server = cfg
            .list_defined_servers()
            .iter()
            .find(|s| s.name == "notes")
            .expect("notes server defined after add");
        assert_eq!(server.command, "notes-mcp");
        assert_eq!(server.args, vec!["serve".to_string()]);
        assert_eq!(server.namespace.as_deref(), Some("nt"));

        // And `list` reflects it, with its command and enabled status.
        let listed = out_string(|o| mcp_list(&path, &[], "tui", o));
        assert!(listed.contains("notes"), "list names the server: {listed}");
        assert!(
            listed.contains("notes-mcp"),
            "list shows the command: {listed}"
        );
        assert!(
            listed.contains("enabled"),
            "an added+enabled server lists as enabled: {listed}"
        );
    }

    #[test]
    fn add_server_without_enabled_leaves_surface_off() {
        let (_dir, path) = temp_cfg();
        mcp_add_server(
            &path,
            "notes",
            "notes-mcp",
            &[],
            None,
            &["tui".to_string()],
            false,
            &mut Vec::new(),
        )
        .expect("add");

        let cfg = ClientMcpConfig::load(&path);
        assert!(
            cfg.list_defined_servers().iter().any(|s| s.name == "notes"),
            "server is defined"
        );
        assert!(
            !cfg.surface_enabled_names("tui")
                .iter()
                .any(|n| n == "notes"),
            "without --enabled the server is not turned on for the surface"
        );
    }

    #[test]
    fn enable_disable_toggles_surface() {
        let (_dir, path) = temp_cfg();
        mcp_add_server(
            &path,
            "notes",
            "notes-mcp",
            &[],
            None,
            &["tui".to_string()],
            false,
            &mut Vec::new(),
        )
        .expect("add");

        mcp_set_enabled(&path, "notes", "tui", true, &[], &mut Vec::new()).expect("enable");
        assert!(
            ClientMcpConfig::load(&path)
                .surface_enabled_names("tui")
                .iter()
                .any(|n| n == "notes"),
            "enable adds the server to the tui surface"
        );

        mcp_set_enabled(&path, "notes", "tui", false, &[], &mut Vec::new()).expect("disable");
        assert!(
            !ClientMcpConfig::load(&path)
                .surface_enabled_names("tui")
                .iter()
                .any(|n| n == "notes"),
            "disable removes the server from the tui surface"
        );
    }

    #[test]
    fn remove_server_removes_it() {
        let (_dir, path) = temp_cfg();
        mcp_add_server(
            &path,
            "notes",
            "notes-mcp",
            &[],
            None,
            &["tui".to_string()],
            true,
            &mut Vec::new(),
        )
        .expect("add");
        assert!(
            ClientMcpConfig::load(&path)
                .list_defined_servers()
                .iter()
                .any(|s| s.name == "notes")
        );

        mcp_remove_server(&path, "notes", &mut Vec::new()).expect("remove");
        let cfg = ClientMcpConfig::load(&path);
        assert!(
            !cfg.list_defined_servers().iter().any(|s| s.name == "notes"),
            "the definition is gone after remove"
        );
        assert!(
            !cfg.surface_enabled_names("tui")
                .iter()
                .any(|n| n == "notes"),
            "remove prunes the surface enable entry too"
        );
    }

    #[test]
    fn remove_absent_server_errors() {
        let (_dir, path) = temp_cfg();
        let err = mcp_remove_server(&path, "ghost", &mut Vec::new())
            .expect_err("removing an undefined server errors");
        assert!(
            err.to_string().contains("ghost"),
            "the error names the missing server: {err}"
        );
    }

    #[test]
    fn list_marks_builtin_overridden_when_same_name_enabled() {
        let (_dir, path) = temp_cfg();
        // A client-MCP server named "fileio", enabled for tui, shadows the
        // built-in of the same name (external > built-in).
        mcp_add_server(
            &path,
            "fileio",
            "my-fileio",
            &[],
            None,
            &["tui".to_string()],
            true,
            &mut Vec::new(),
        )
        .expect("add");

        let builtins = [
            BuiltinInfo::new("fileio", 7),
            BuiltinInfo::new("terminal", 4),
        ];
        let listed = out_string(|o| mcp_list(&path, &builtins, "tui", o));

        // The overriding built-in renders as overridden; the untouched one does not.
        assert!(
            listed.contains("fileio") && listed.contains("overridden"),
            "an overridden built-in is marked overridden: {listed}"
        );
        // The built-in section shows tool counts.
        assert!(
            listed.contains('7') && listed.contains('4'),
            "built-in tool counts are shown: {listed}"
        );
        // "terminal" is not overridden (no same-named client-MCP server).
        let terminal_line = listed
            .lines()
            .find(|l| l.contains("terminal"))
            .expect("terminal built-in listed");
        assert!(
            !terminal_line.contains("overridden"),
            "a non-shadowed built-in is not marked overridden: {terminal_line}"
        );
    }

    #[test]
    fn disable_builtin_persists_to_config() {
        let (_dir, path) = temp_cfg();
        let builtins = [BuiltinInfo::new("fileio", 7)];
        // "fileio" exists only as a built-in (no client-MCP definition). `on =
        // false` disables it, so it is added to the surface's disabled_builtins.
        let msg = out_string(|o| mcp_set_enabled(&path, "fileio", "tui", false, &builtins, o));
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("fileio") && lower.contains("disabled") && lower.contains("tui"),
            "disabling a built-in confirms name + state + surface: {msg}"
        );
        assert_eq!(
            ClientMcpConfig::load(&path).surface_disabled_builtins("tui"),
            &["fileio"],
            "disable writes the built-in to the surface's disabled_builtins list"
        );
    }

    #[test]
    fn enable_builtin_removes_from_disabled() {
        let (_dir, path) = temp_cfg();
        let builtins = [BuiltinInfo::new("web", 3)];
        // Disable, then re-enable: the built-in returns to its default-on state.
        mcp_set_enabled(&path, "web", "tui", false, &builtins, &mut Vec::new()).expect("disable");
        assert_eq!(
            ClientMcpConfig::load(&path).surface_disabled_builtins("tui"),
            &["web"]
        );

        let msg = out_string(|o| mcp_set_enabled(&path, "web", "tui", true, &builtins, o));
        let lower = msg.to_lowercase();
        assert!(
            lower.contains("web") && lower.contains("enabled"),
            "re-enabling a built-in confirms it: {msg}"
        );
        assert!(
            ClientMcpConfig::load(&path)
                .surface_disabled_builtins("tui")
                .is_empty(),
            "enable removes the built-in from the surface's disabled_builtins list"
        );
    }

    #[test]
    fn disable_builtin_is_per_surface() {
        let (_dir, path) = temp_cfg();
        let builtins = [BuiltinInfo::new("terminal", 4)];
        mcp_set_enabled(&path, "terminal", "tui", false, &builtins, &mut Vec::new())
            .expect("disable");
        let cfg = ClientMcpConfig::load(&path);
        assert_eq!(cfg.surface_disabled_builtins("tui"), &["terminal"]);
        assert!(
            cfg.surface_disabled_builtins("gtk").is_empty(),
            "disabling a built-in for one surface leaves the others on"
        );
    }

    #[test]
    fn list_marks_builtin_disabled_by_config() {
        let (_dir, path) = temp_cfg();
        let builtins = [BuiltinInfo::new("web", 3), BuiltinInfo::new("fileio", 7)];
        mcp_set_enabled(&path, "web", "tui", false, &builtins, &mut Vec::new()).expect("disable");

        let listed = out_string(|o| mcp_list(&path, &builtins, "tui", o));
        // The config-disabled built-in is marked as such.
        let web_line = listed
            .lines()
            .find(|l| l.contains("web"))
            .expect("web built-in listed");
        assert!(
            web_line.contains("disabled (config)"),
            "a config-disabled built-in is marked in list: {web_line}"
        );
        // A built-in that was not disabled still lists as active.
        let fileio_line = listed
            .lines()
            .find(|l| l.contains("fileio"))
            .expect("fileio built-in listed");
        assert!(
            fileio_line.contains("active"),
            "an untouched built-in stays active: {fileio_line}"
        );
    }

    #[test]
    fn defined_server_named_like_builtin_takes_server_path() {
        // A name that is BOTH a defined client-MCP server AND a built-in must
        // resolve to the server (surface enable list), never the built-in
        // disabled_builtins path — the precedence the interactive host applies.
        let (_dir, path) = temp_cfg();
        mcp_add_server(
            &path,
            "fileio",
            "my-fileio",
            &[],
            None,
            &["tui".to_string()],
            true,
            &mut Vec::new(),
        )
        .expect("add");
        let builtins = [BuiltinInfo::new("fileio", 7)];

        mcp_set_enabled(&path, "fileio", "tui", false, &builtins, &mut Vec::new())
            .expect("disable");
        let cfg = ClientMcpConfig::load(&path);
        // The server was disabled for the surface (server path)...
        assert!(
            !cfg.surface_enabled_names("tui")
                .iter()
                .any(|n| n == "fileio"),
            "the defined server is the one toggled off"
        );
        // ...and the built-in disabled list was NOT touched.
        assert!(
            cfg.surface_disabled_builtins("tui").is_empty(),
            "a name matching a defined server never writes disabled_builtins"
        );
    }

    #[test]
    fn enable_unknown_name_errors() {
        let (_dir, path) = temp_cfg();
        let err = mcp_set_enabled(&path, "ghost", "tui", true, &[], &mut Vec::new())
            .expect_err("enabling an unknown, non-built-in name errors");
        assert!(
            err.to_string().contains("ghost"),
            "the error names the unknown server: {err}"
        );
    }

    #[test]
    fn show_reports_added_server_and_path() {
        let (_dir, path) = temp_cfg();
        mcp_add_server(
            &path,
            "notes",
            "notes-mcp",
            &[],
            None,
            &["tui".to_string()],
            true,
            &mut Vec::new(),
        )
        .expect("add");

        let shown = out_string(|o| config_show(&path, None, o));
        assert!(shown.contains("notes"), "show renders the server: {shown}");
        assert!(
            shown.contains("client-mcp.toml"),
            "show names the config path: {shown}"
        );
    }

    #[test]
    fn show_rejects_unknown_section() {
        let (_dir, path) = temp_cfg();
        let err = config_show(&path, Some("bogus"), &mut Vec::new())
            .expect_err("an unknown --section is rejected");
        assert!(
            err.to_string().contains("bogus") || err.to_string().contains("mcp"),
            "the error explains only mcp is supported: {err}"
        );
    }

    #[test]
    fn path_prints_config_location() {
        let (_dir, path) = temp_cfg();
        let printed = out_string(|o| config_path(&path, o));
        assert!(
            printed.contains("client-mcp.toml"),
            "path prints the config location: {printed}"
        );
    }

    // --- Client preferences: share-client-context (da#549 Phase 2b) ---

    /// A fresh tempdir + a settings.json path inside it (the file does not yet
    /// exist, exercising the load-absent-as-default path).
    fn temp_settings() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        (dir, path)
    }

    #[test]
    fn set_share_client_context_off_persists_and_reloads() {
        let (_dir, path) = temp_settings();
        let out = out_string(|o| config_set(&path, SHARE_CLIENT_CONTEXT_KEY, "off", o));
        assert!(out.contains("off"), "set echoes the new value: {out}");
        // The change survives a reload (and nothing else in the file matters).
        assert!(!Settings::load_from(&path).share_client_context);
    }

    #[test]
    fn set_share_client_context_on_persists_and_reloads() {
        let (_dir, path) = temp_settings();
        // Turn it off, then back on, to prove `on` actually writes true.
        config_set(&path, SHARE_CLIENT_CONTEXT_KEY, "off", &mut Vec::new()).expect("off");
        let out = out_string(|o| config_set(&path, SHARE_CLIENT_CONTEXT_KEY, "on", o));
        assert!(out.contains("on"), "set echoes the new value: {out}");
        assert!(Settings::load_from(&path).share_client_context);
    }

    #[test]
    fn set_preserves_other_settings() {
        let (_dir, path) = temp_settings();
        // Seed a non-default show_debug, then flip share-client-context.
        Settings {
            show_debug: true,
            share_client_context: true,
        }
        .save_to(&path)
        .expect("seed");
        config_set(&path, SHARE_CLIENT_CONTEXT_KEY, "off", &mut Vec::new()).expect("set");
        let back = Settings::load_from(&path);
        assert!(
            back.show_debug,
            "flipping one setting must not clobber another"
        );
        assert!(!back.share_client_context);
    }

    #[test]
    fn set_accepts_true_false_yes_no_one_zero() {
        let (_dir, path) = temp_settings();
        for on in ["on", "true", "yes", "1", "ON", "True"] {
            config_set(&path, SHARE_CLIENT_CONTEXT_KEY, on, &mut Vec::new()).expect("on-ish");
            assert!(
                Settings::load_from(&path).share_client_context,
                "{on} => true"
            );
        }
        for off in ["off", "false", "no", "0", "OFF", "False"] {
            config_set(&path, SHARE_CLIENT_CONTEXT_KEY, off, &mut Vec::new()).expect("off-ish");
            assert!(
                !Settings::load_from(&path).share_client_context,
                "{off} => false"
            );
        }
    }

    #[test]
    fn set_rejects_invalid_value() {
        let (_dir, path) = temp_settings();
        let err = config_set(&path, SHARE_CLIENT_CONTEXT_KEY, "maybe", &mut Vec::new())
            .expect_err("an unparseable value is rejected");
        assert!(
            err.to_string().contains("maybe") || err.to_string().contains("on/off"),
            "the error explains the accepted values: {err}"
        );
        // A rejected value must not have written the file.
        assert!(
            !path.exists(),
            "a rejected set must not create the settings file"
        );
    }

    #[test]
    fn set_rejects_unknown_key() {
        let (_dir, path) = temp_settings();
        let err = config_set(&path, "no-such-setting", "on", &mut Vec::new())
            .expect_err("an unknown key is rejected");
        assert!(
            err.to_string().contains("no-such-setting")
                || err.to_string().contains(SHARE_CLIENT_CONTEXT_KEY),
            "the error names the offending/known key: {err}"
        );
    }

    #[test]
    fn get_reports_current_value() {
        let (_dir, path) = temp_settings();
        // Absent file => default on.
        let on = out_string(|o| config_get(&path, SHARE_CLIENT_CONTEXT_KEY, o));
        assert!(on.contains("on"), "get reports the default (on): {on}");
        config_set(&path, SHARE_CLIENT_CONTEXT_KEY, "off", &mut Vec::new()).expect("off");
        let off = out_string(|o| config_get(&path, SHARE_CLIENT_CONTEXT_KEY, o));
        assert!(
            off.contains("off"),
            "get reflects the persisted value: {off}"
        );
    }

    #[test]
    fn get_rejects_unknown_key() {
        let (_dir, path) = temp_settings();
        let err = config_get(&path, "no-such-setting", &mut Vec::new())
            .expect_err("an unknown key is rejected");
        assert!(err.to_string().contains("no-such-setting") || err.to_string().contains("known"));
    }
}
