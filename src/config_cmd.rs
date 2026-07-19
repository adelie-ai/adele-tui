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
//! Scope: this manages the **client-side** MCP config only. Daemon-hosted MCP
//! servers are out of this first cut (they need a live connection + the typed
//! command channel the `F5` panel uses); [`mcp_list`] says so in its output.
//!
//! [`default_client_mcp_path`]: desktop_assistant_client_common::mcp_host::default_client_mcp_path

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use desktop_assistant_client_common::mcp_host::{ClientMcpConfig, McpServerConfig};

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
/// built-ins (with tool counts, marking any overridden by a same-named,
/// surface-enabled client-MCP server).
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
            let status = if overriding.iter().any(|n| *n == builtin.name) {
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

/// `config mcp enable`/`disable`: flip a client-MCP server's membership in one
/// surface's enable list. A name that matches only a built-in is reported as
/// not-yet-supported (a normal informational outcome, not an error); a name that
/// matches neither is an error.
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
        // A name that matches only a built-in: toggling built-ins needs the
        // built-in disabled-state (da#538 slice 4), which doesn't exist yet.
        // Report it as a normal informational decline, not an error, and make no
        // change rather than writing a dangling surface entry.
        writeln!(
            out,
            "'{name}' is a built-in (in-process) server; enabling/disabling built-ins is not yet supported (coming with the panel toggle). No change made."
        )?;
    } else {
        bail!("no such client-MCP server: '{name}'");
    }
    Ok(())
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
    fn enable_builtin_only_name_is_declined_not_errored() {
        let (_dir, path) = temp_cfg();
        let builtins = [BuiltinInfo::new("fileio", 7)];
        // "fileio" exists only as a built-in (no client-MCP definition).
        let msg = out_string(|o| mcp_set_enabled(&path, "fileio", "tui", true, &builtins, o));
        assert!(
            msg.contains("built-in") && msg.contains("not yet supported"),
            "toggling a built-in-only name is declined with a clear message: {msg}"
        );
        // Nothing was written to the surface list.
        assert!(
            !ClientMcpConfig::load(&path)
                .surface_enabled_names("tui")
                .iter()
                .any(|n| n == "fileio"),
            "a declined built-in toggle does not mutate the config"
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
}
