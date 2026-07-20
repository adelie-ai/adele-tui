//! Compiled-in ("built-in") MCP servers hosted in-process (da#538 Phase C/D).
//!
//! The core set (fileio/terminal/tasks/web) is compiled in and hosted by
//! default so a fresh tui is useful with no `client-mcp.toml`. A second,
//! opt-in "broad set" (weather/internet-radio/openstreetmap/geocode/skills) is
//! **off by default**: each links in only under its own `mcp-*` feature or the
//! `builtin-extras` umbrella, so the stock build links only the core four and
//! behaves exactly as before. (A future mac client is expected to turn
//! `builtin-extras` on in its own default.) An external
//! client-mcp server of the SAME NAME overrides (suppresses) the built-in
//! (external > built-in); that override decision now lives centrally in
//! [`McpHost::start_with`], which skips + logs a shadowed built-in and reports
//! it via [`McpHost::builtin_status`]. This module just enumerates the full
//! compiled-in set and maps that status into the panel's view-model DTO.
//!
//! [`McpHost::start_with`]: desktop_assistant_client_common::mcp_host::McpHost::start_with
//! [`McpHost::builtin_status`]: desktop_assistant_client_common::mcp_host::McpHost::builtin_status

use client_ui_common::BuiltinServerDto;
use desktop_assistant_client_common::mcp_host::{BuiltinServer, BuiltinStatus};
#[cfg(any(
    feature = "mcp-fileio",
    feature = "mcp-terminal",
    feature = "mcp-tasks",
    feature = "mcp-web",
    feature = "mcp-weather",
    feature = "mcp-internet-radio",
    feature = "mcp-openstreetmap",
    feature = "mcp-geocode",
    feature = "mcp-skills"
))]
use std::sync::Arc;

/// Build every enabled built-in server as the full compiled-in set.
///
/// The override (skipping a built-in whose name matches a configured client-mcp
/// server) is owned by [`McpHost::start_with`], so this returns the complete set
/// and lets the host make and log the shadowing decision.
///
/// Each `#[cfg]` block compiles in only when its `mcp-*` feature is on, so a
/// `--no-default-features` build hosts nothing and the tui behaves as it did
/// before Phase C. The infallible constructors (fileio, web, and all five
/// opt-in broad-set extras) are always registered; the fallible core ones
/// (terminal, tasks) are logged and skipped if their zero-config constructor
/// fails, so a broken environment degrades to the remaining tools rather than
/// losing the whole set.
///
/// [`McpHost::start_with`]: desktop_assistant_client_common::mcp_host::McpHost::start_with
pub fn builtin_servers() -> Vec<BuiltinServer> {
    #[allow(unused_mut)]
    let mut out: Vec<BuiltinServer> = Vec::new();

    #[cfg(feature = "mcp-fileio")]
    out.push(BuiltinServer::new(
        "fileio",
        "fileio",
        Arc::new(fileio_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-terminal")]
    match terminal_mcp::build_service() {
        Ok(svc) => out.push(BuiltinServer::new("terminal", "terminal", Arc::new(svc))),
        Err(e) => tracing::warn!("built-in terminal server unavailable: {e}"),
    }
    #[cfg(feature = "mcp-tasks")]
    match tasks_mcp::build_service() {
        Ok(svc) => out.push(BuiltinServer::new("tasks", "tasks", Arc::new(svc))),
        Err(e) => tracing::warn!("built-in tasks server unavailable: {e}"),
    }
    #[cfg(feature = "mcp-web")]
    out.push(BuiltinServer::new(
        "web",
        "web",
        Arc::new(web_mcp::build_service()),
    ));

    // The opt-in "broad set" extras (da#538), each off unless its `mcp-*` feature
    // (or the `builtin-extras` umbrella) is enabled. All five have infallible
    // `build_service()` constructors, so they always register when compiled in.
    // Each uses its fleet-canonical name for both `name` and `namespace` (the
    // `name` from the daemon's `deploy/mcp/mcp_servers.default.toml`, e.g.
    // `weather-forecast`), so an in-process built-in, the standalone binary, and a
    // same-named external override all share one namespace and interchange cleanly.
    #[cfg(feature = "mcp-weather")]
    out.push(BuiltinServer::new(
        "weather-forecast",
        "weather-forecast",
        Arc::new(weather_forecast_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-internet-radio")]
    out.push(BuiltinServer::new(
        "internet-radio",
        "internet-radio",
        Arc::new(internet_radio_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-openstreetmap")]
    out.push(BuiltinServer::new(
        "openstreetmap",
        "openstreetmap",
        Arc::new(openstreetmap_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-geocode")]
    out.push(BuiltinServer::new(
        "geocode",
        "geocode",
        Arc::new(geocode_mcp::build_service()),
    ));
    #[cfg(feature = "mcp-skills")]
    out.push(BuiltinServer::new(
        "skills",
        "skills",
        Arc::new(skills_mcp::build_service()),
    ));

    out
}

/// Map the host's per-built-in [`BuiltinStatus`] into the view-model
/// [`BuiltinServerDto`]s the shared MCP-servers panel renders via
/// `client_ui_common::server_rows_with_builtins`. The `usize` `tool_count`
/// widens to the DTO's `u32`; `overridden_by` and `disabled_by_config` carry
/// straight through so a built-in that is overridden or explicitly turned off in
/// this client's config renders as a disabled row (da#538 slice 4).
pub fn builtin_dtos(status: Vec<BuiltinStatus>) -> Vec<BuiltinServerDto> {
    status
        .into_iter()
        .map(|s| BuiltinServerDto {
            name: s.name,
            namespace: s.namespace,
            tool_count: s.tool_count as u32,
            overridden_by: s.overridden_by,
            disabled_by_config: s.disabled_by_config,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use client_ui_common::{ServerKind, kind_label, server_rows_with_builtins};

    /// fileio's constructor is infallible, so the compiled-in set deterministically
    /// contains a server named "fileio", advertised under the "fileio" namespace.
    /// The override (skipping a shadowed built-in) now lives centrally in
    /// [`McpHost::start_with`], so `builtin_servers()` always returns the full set.
    #[cfg(feature = "mcp-fileio")]
    #[test]
    fn fileio_builtin_present_and_namespaced_in_full_set() {
        let servers = builtin_servers();
        let fileio = servers
            .iter()
            .find(|s| s.name == "fileio")
            .expect("fileio built-in must be present in the compiled set");
        assert_eq!(
            fileio.namespace, "fileio",
            "fileio built-in must be advertised under the 'fileio' namespace"
        );
    }

    /// With the opt-in "broad set" extras compiled in (the `builtin-extras`
    /// umbrella), the full built-in set additionally contains each of the five
    /// broad-set servers, every one advertised under its own canonical
    /// namespace so an in-process server is indistinguishable from the
    /// standalone binary (da#538). Gated on all five `mcp-*` extras features, so
    /// it exercises only the extras build and leaves the default (core-only)
    /// build's expectations untouched.
    #[cfg(all(
        feature = "mcp-weather",
        feature = "mcp-internet-radio",
        feature = "mcp-openstreetmap",
        feature = "mcp-geocode",
        feature = "mcp-skills"
    ))]
    #[test]
    fn builtin_extras_present_and_namespaced_in_full_set() {
        let servers = builtin_servers();
        for name in [
            "weather-forecast",
            "internet-radio",
            "openstreetmap",
            "geocode",
            "skills",
        ] {
            let server = servers.iter().find(|s| s.name == name).unwrap_or_else(|| {
                panic!("broad-set built-in {name:?} must be present under the extras build")
            });
            assert_eq!(
                server.namespace, name,
                "broad-set built-in {name:?} must be advertised under the {name:?} namespace"
            );
        }
    }

    /// The panel mapping: a host [`BuiltinStatus`] list becomes [`BuiltinServerDto`]s
    /// that `server_rows_with_builtins` turns into an active built-in row (no
    /// disabled reason) and an overridden one (a disabled row whose reason names the
    /// external server). This is the exact path the F5 MCP panel renders.
    #[test]
    fn builtin_dtos_map_to_active_and_overridden_rows() {
        let status = vec![
            BuiltinStatus {
                name: "fileio".into(),
                namespace: "fileio".into(),
                tool_count: 7,
                overridden_by: None,
                disabled_by_config: false,
            },
            BuiltinStatus {
                name: "web".into(),
                namespace: "web".into(),
                tool_count: 3,
                overridden_by: Some("web".into()),
                disabled_by_config: false,
            },
        ];

        let dtos = builtin_dtos(status);
        assert_eq!(dtos.len(), 2, "each status maps to exactly one dto");
        let fileio_dto = dtos
            .iter()
            .find(|d| d.name == "fileio")
            .expect("fileio dto present");
        assert_eq!(fileio_dto.tool_count, 7, "usize tool_count widens to u32");
        assert_eq!(fileio_dto.overridden_by, None);

        let rows = server_rows_with_builtins(&[], &[], &dtos);

        let fileio = rows
            .iter()
            .find(|r| r.name == "fileio")
            .expect("fileio row present");
        assert_eq!(
            fileio.kind,
            ServerKind::BuiltIn,
            "built-in rows carry the BuiltIn kind"
        );
        assert_eq!(
            fileio.disabled_reason, None,
            "an active built-in is not disabled"
        );
        assert_eq!(kind_label(fileio.kind), "built-in");

        let web = rows
            .iter()
            .find(|r| r.name == "web")
            .expect("web row present");
        assert_eq!(web.kind, ServerKind::BuiltIn);
        let reason = web
            .disabled_reason
            .as_deref()
            .expect("an overridden built-in must render disabled with a reason");
        assert!(
            reason.contains("overridden"),
            "reason explains the override: {reason}"
        );
        assert!(
            reason.contains("web"),
            "reason names the overriding server: {reason}"
        );
    }

    /// A built-in explicitly turned off in this client's config maps to a DTO
    /// carrying `disabled_by_config`, which `server_rows_with_builtins` renders
    /// as a disabled row whose reason names the config off-switch (and takes
    /// precedence over any override reason). This is the F5 panel's display path
    /// for a config-disabled built-in (da#538 slice 4).
    #[test]
    fn config_disabled_builtin_maps_to_disabled_row() {
        let status = vec![BuiltinStatus {
            name: "web".into(),
            namespace: "web".into(),
            tool_count: 3,
            // Both flags set at once: config-disable must win the display.
            overridden_by: Some("web".into()),
            disabled_by_config: true,
        }];

        let dtos = builtin_dtos(status);
        assert!(
            dtos[0].disabled_by_config,
            "the config-disabled flag carries into the DTO"
        );

        let rows = server_rows_with_builtins(&[], &[], &dtos);
        let web = rows.iter().find(|r| r.name == "web").expect("web row");
        assert_eq!(web.kind, ServerKind::BuiltIn);
        let reason = web
            .disabled_reason
            .as_deref()
            .expect("a config-disabled built-in renders disabled with a reason");
        assert!(
            reason.contains("config"),
            "the config-disable reason wins the display: {reason}"
        );
    }
}
