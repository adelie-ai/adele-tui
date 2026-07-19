//! Compiled-in ("built-in") MCP servers hosted in-process (da#538 Phase C/D).
//!
//! The core set (fileio/terminal/tasks/web) is compiled in and hosted by
//! default so a fresh tui is useful with no `client-mcp.toml`. An external
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
    feature = "mcp-web"
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
/// before Phase C. The infallible constructors (fileio, web) are always
/// registered; the fallible ones (terminal, tasks) are logged and skipped if
/// their zero-config constructor fails, so a broken environment degrades to the
/// remaining tools rather than losing the whole set.
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

    out
}

/// Map the host's per-built-in [`BuiltinStatus`] into the view-model
/// [`BuiltinServerDto`]s the shared MCP-servers panel renders via
/// `client_ui_common::server_rows_with_builtins`. The `usize` `tool_count`
/// widens to the DTO's `u32`; `overridden_by` carries straight through so an
/// overridden built-in renders as a disabled row.
pub fn builtin_dtos(status: Vec<BuiltinStatus>) -> Vec<BuiltinServerDto> {
    status
        .into_iter()
        .map(|s| BuiltinServerDto {
            name: s.name,
            namespace: s.namespace,
            tool_count: s.tool_count as u32,
            overridden_by: s.overridden_by,
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
            },
            BuiltinStatus {
                name: "web".into(),
                namespace: "web".into(),
                tool_count: 3,
                overridden_by: Some("web".into()),
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
}
