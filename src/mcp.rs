//! Spec (failing tests) for the MCP-servers admin panel (desktop-assistant#495).
//!
//! Written before the implementation (TDD): with the module body absent, these
//! tests reference `super::` items that do not exist yet, so `cargo test` fails
//! to compile. The implementation commit that follows makes them pass.

#[cfg(test)]
mod tests {
    use super::*;
    use desktop_assistant_api_model::{McpServerView, ServiceAccountView};
    use ratatui::{Terminal, backend::TestBackend};

    fn stdio(name: &str) -> McpForm {
        McpForm {
            name: name.into(),
            command: "fileio-mcp".into(),
            ..McpForm::blank(McpTransport::Stdio)
        }
    }

    fn http(name: &str) -> McpForm {
        McpForm {
            name: name.into(),
            url: "https://x.example/mcp".into(),
            ..McpForm::blank(McpTransport::Http)
        }
    }

    // --- status_display -------------------------------------------------------

    #[test]
    fn status_display_covers_all_six_states() {
        assert_eq!(status_display("running"), (StatusTone::Ok, "Running"));
        assert_eq!(status_display("stopped"), (StatusTone::Neutral, "Stopped"));
        assert_eq!(
            status_display("disabled"),
            (StatusTone::Neutral, "Disabled")
        );
        assert_eq!(
            status_display("needs_auth"),
            (StatusTone::Warn, "Sign in required")
        );
        assert_eq!(
            status_display("auth_expired"),
            (StatusTone::Warn, "Sign in expired")
        );
        assert_eq!(status_display("error"), (StatusTone::Error, "Error"));
    }

    #[test]
    fn status_display_unknown_is_neutral() {
        assert_eq!(
            status_display("teleporting"),
            (StatusTone::Neutral, "Unknown")
        );
        assert_eq!(status_display(""), (StatusTone::Neutral, "Unknown"));
    }

    // --- transport_chip -------------------------------------------------------

    #[test]
    fn transport_chip_http_is_remote_else_local() {
        assert_eq!(transport_chip("http"), "remote");
        assert_eq!(transport_chip("stdio"), "local");
        assert_eq!(transport_chip("something-new"), "local");
    }

    // --- parse_env ------------------------------------------------------------

    #[test]
    fn parse_env_reads_key_value_lines_in_order() {
        assert_eq!(
            parse_env("TOKEN=abc\nDEBUG=1"),
            vec![
                ("TOKEN".to_string(), "abc".to_string()),
                ("DEBUG".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn parse_env_skips_blank_and_malformed_lines() {
        assert_eq!(
            parse_env("\n  \nNOVALUE\n=novalue\nOK=1\n"),
            vec![("OK".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn parse_env_value_may_contain_equals() {
        assert_eq!(
            parse_env("QUERY=a=b=c"),
            vec![("QUERY".to_string(), "a=b=c".to_string())]
        );
    }

    #[test]
    fn parse_env_trims_key_and_value() {
        assert_eq!(
            parse_env("  KEY = val \n"),
            vec![("KEY".to_string(), "val".to_string())]
        );
    }

    // --- split_args / split_scopes -------------------------------------------

    #[test]
    fn split_args_splits_on_whitespace_runs() {
        assert_eq!(
            split_args("serve   --root  /data"),
            vec!["serve", "--root", "/data"]
        );
    }

    #[test]
    fn split_args_empty_is_empty() {
        assert!(split_args("   ").is_empty());
        assert!(split_args("").is_empty());
    }

    #[test]
    fn split_scopes_splits_on_whitespace_and_commas() {
        assert_eq!(split_scopes("a b,c ,  d"), vec!["a", "b", "c", "d"]);
        assert!(split_scopes("").is_empty());
    }

    // --- bearer_secret_ref ----------------------------------------------------

    #[test]
    fn bearer_secret_ref_appends_token_suffix() {
        assert_eq!(bearer_secret_ref("gmail"), "gmail_token");
    }

    // --- sanitize (control-char stripping) ------------------------------------

    #[test]
    fn sanitize_replaces_control_chars_with_space() {
        // An embedded ESC + newline + tab must not survive to the terminal.
        assert_eq!(sanitize("a\x1b[2Jb\nc\td"), "a [2Jb c d");
        assert_eq!(sanitize("plain-name_1"), "plain-name_1");
    }

    // --- mcp_secret_command (wire shape + redaction) --------------------------

    #[test]
    fn mcp_secret_command_wire_shape() {
        let cmd = mcp_secret_command("gmail_token".into(), "ya29.tok".into());
        let json = serde_json::to_string(&cmd).expect("serializes");
        assert_eq!(
            json,
            r#"{"set_mcp_secret":{"id":"gmail_token","value":"ya29.tok"}}"#
        );
    }

    #[test]
    fn mcp_secret_command_redacts_value_in_debug() {
        let cmd = mcp_secret_command("gmail_token".into(), "ya29.supersecret".into());
        let dump = format!("{cmd:?}");
        assert!(!dump.contains("ya29.supersecret"), "token leaked: {dump}");
    }

    // --- build: stdio ---------------------------------------------------------

    #[test]
    fn build_stdio_emits_exact_config_json() {
        let form = McpForm {
            args: "serve --root /data".into(),
            namespace: "files".into(),
            env: "TOKEN=abc\nDEBUG=1".into(),
            ..stdio("files")
        };
        let built = form.build().expect("builds");
        assert!(!built.editing);
        assert_eq!(built.name, "files");
        assert_eq!(built.secret, None);
        // env is a BTreeMap in the DTO → keys sorted (DEBUG before TOKEN).
        assert_eq!(
            built.config_json,
            r#"{"name":"files","enabled":true,"command":"fileio-mcp","args":["serve","--root","/data"],"namespace":"files","env":{"DEBUG":"1","TOKEN":"abc"}}"#
        );
    }

    #[test]
    fn build_stdio_omits_empty_optionals() {
        let built = stdio("bare").build().expect("builds");
        assert_eq!(
            built.config_json,
            r#"{"name":"bare","enabled":true,"command":"fileio-mcp"}"#
        );
    }

    #[test]
    fn build_carries_disabled_flag() {
        let form = McpForm {
            enabled: false,
            ..stdio("x")
        };
        let built = form.build().expect("builds");
        assert!(built.config_json.contains(r#""enabled":false"#));
    }

    // --- build: http bearer ---------------------------------------------------

    #[test]
    fn build_http_bearer_emits_config_and_secret() {
        let form = McpForm {
            url: "https://gmailmcp.googleapis.com/mcp/v1".into(),
            auth: McpAuthKind::Bearer,
            bearer_token: "  ya29.token \n".into(),
            ..http("gmail")
        };
        let built = form.build().expect("builds");
        assert_eq!(
            built.config_json,
            r#"{"name":"gmail","enabled":true,"http":{"url":"https://gmailmcp.googleapis.com/mcp/v1","auth_bearer_secret":"gmail_token"}}"#
        );
        // The token is trimmed and written under the `{name}_token` ref.
        assert_eq!(
            built.secret,
            Some(("gmail_token".to_string(), "ya29.token".to_string()))
        );
    }

    #[test]
    fn build_http_bearer_blank_token_writes_no_secret() {
        // Write-only: a blank token never wipes a stored token — but the config
        // still references the ref so the server stays honestly "bearer".
        let form = McpForm {
            auth: McpAuthKind::Bearer,
            bearer_token: "   ".into(),
            ..http("gmail")
        };
        let built = form.build().expect("builds");
        assert_eq!(built.secret, None);
        assert!(
            built
                .config_json
                .contains(r#""auth_bearer_secret":"gmail_token""#)
        );
    }

    // --- build: http oauth ----------------------------------------------------

    #[test]
    fn build_http_oauth_emits_account_ref_and_scopes() {
        let form = McpForm {
            url: "https://cal.example/mcp".into(),
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            scopes: "calendar.read, calendar.write".into(),
            ..http("cal")
        };
        let built = form.build().expect("builds");
        // OAuth carries only the account ref + scopes — never a secret value.
        assert_eq!(built.secret, None);
        assert_eq!(
            built.config_json,
            r#"{"name":"cal","enabled":true,"http":{"url":"https://cal.example/mcp","oauth_account":"work-google","scopes":["calendar.read","calendar.write"]}}"#
        );
    }

    // --- build: validation ----------------------------------------------------

    #[test]
    fn build_requires_command_for_stdio() {
        let form = McpForm {
            command: "   ".into(),
            ..stdio("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_url_for_http() {
        let form = McpForm {
            url: "".into(),
            ..http("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_account_for_oauth() {
        let form = McpForm {
            auth: McpAuthKind::OAuth,
            oauth_account: "  ".into(),
            ..http("x")
        };
        assert!(form.build().is_err());
    }

    #[test]
    fn build_requires_valid_name_on_create() {
        assert!(stdio("").build().is_err());
        assert!(stdio("has space").build().is_err());
        assert!(stdio("ok-name_1").build().is_ok());
    }

    #[test]
    fn build_edit_does_not_revalidate_locked_name() {
        // On edit the name is the already-stored (locked) one, so build trusts
        // it rather than re-running the create-time slug check.
        let form = McpForm {
            editing: true,
            name: "already.there".into(),
            ..stdio("already.there")
        };
        let built = form.build().expect("builds");
        assert!(built.editing);
        assert_eq!(built.name, "already.there");
    }

    // --- from_view (edit prefill) --------------------------------------------

    #[test]
    fn from_view_prefills_stdio_editor() {
        let view = McpServerView {
            name: "files".into(),
            command: "fileio-mcp".into(),
            args: vec!["serve".into(), "--root".into(), "/data".into()],
            namespace: Some("files".into()),
            enabled: true,
            status: "running".into(),
            transport: "stdio".into(),
            target: "fileio-mcp".into(),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert!(f.editing);
        assert_eq!(f.transport, McpTransport::Stdio);
        assert_eq!(f.name, "files");
        assert_eq!(f.command, "fileio-mcp");
        assert_eq!(f.args, "serve --root /data");
        assert_eq!(f.namespace, "files");
        // The view carries no env — never pre-filled.
        assert_eq!(f.env, "");
    }

    #[test]
    fn from_view_prefills_http_bearer_editor() {
        let view = McpServerView {
            name: "gh".into(),
            enabled: true,
            status: "running".into(),
            transport: "http".into(),
            target: "https://gh.example/mcp".into(),
            auth_kind: Some("bearer".into()),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::Bearer);
        assert_eq!(f.url, "https://gh.example/mcp");
        // Write-only: the token is never echoed / pre-filled.
        assert_eq!(f.bearer_token, "");
    }

    #[test]
    fn from_view_prefills_http_oauth_editor() {
        let view = McpServerView {
            name: "cal".into(),
            enabled: true,
            status: "needs_auth".into(),
            transport: "http".into(),
            target: "https://cal.example/mcp".into(),
            auth_kind: Some("oauth".into()),
            oauth_account_ref: Some("work-google".into()),
            oauth_scopes: vec!["calendar.read".into()],
            oauth_authorized: Some(false),
            ..Default::default()
        };
        let f = McpForm::from_view(&view);
        assert_eq!(f.transport, McpTransport::Http);
        assert_eq!(f.auth, McpAuthKind::OAuth);
        assert_eq!(f.url, "https://cal.example/mcp");
        assert_eq!(f.oauth_account, "work-google");
        assert_eq!(f.scopes, "calendar.read");
    }

    // --- transport / auth cyclers --------------------------------------------

    #[test]
    fn transport_cycle_wraps_both_ways() {
        assert_eq!(McpTransport::Stdio.cycle(1), McpTransport::Http);
        assert_eq!(McpTransport::Http.cycle(1), McpTransport::Stdio);
        assert_eq!(McpTransport::Stdio.cycle(-1), McpTransport::Http);
    }

    #[test]
    fn auth_cycle_wraps_three_ways() {
        assert_eq!(McpAuthKind::None.cycle(1), McpAuthKind::Bearer);
        assert_eq!(McpAuthKind::Bearer.cycle(1), McpAuthKind::OAuth);
        assert_eq!(McpAuthKind::OAuth.cycle(1), McpAuthKind::None);
        assert_eq!(McpAuthKind::None.cycle(-1), McpAuthKind::OAuth);
    }

    // --- fields_for (transport/auth-divergent field sets) --------------------

    #[test]
    fn fields_for_stdio_has_command_and_env_not_url() {
        let fields = fields_for(McpTransport::Stdio, McpAuthKind::None);
        assert!(fields.contains(&Field::Command));
        assert!(fields.contains(&Field::Env));
        assert!(!fields.contains(&Field::Url));
        assert!(!fields.contains(&Field::Auth));
    }

    #[test]
    fn fields_for_http_bearer_has_token_not_account() {
        let fields = fields_for(McpTransport::Http, McpAuthKind::Bearer);
        assert!(fields.contains(&Field::Url));
        assert!(fields.contains(&Field::BearerToken));
        assert!(!fields.contains(&Field::OAuthAccount));
        assert!(!fields.contains(&Field::Command));
    }

    #[test]
    fn fields_for_http_oauth_has_account_and_scopes() {
        let fields = fields_for(McpTransport::Http, McpAuthKind::OAuth);
        assert!(fields.contains(&Field::OAuthAccount));
        assert!(fields.contains(&Field::Scopes));
        assert!(!fields.contains(&Field::BearerToken));
    }

    // --- FormState round-trip (widget ⇄ pure model) --------------------------

    #[test]
    fn form_state_round_trips_stdio() {
        let pure = McpForm {
            args: "serve --root /data".into(),
            namespace: "files".into(),
            env: "TOKEN=abc\nDEBUG=1".into(),
            ..stdio("files")
        };
        let round = FormState::from_pure(pure.clone()).snapshot();
        assert_eq!(round, pure);
    }

    #[test]
    fn form_state_round_trips_http_bearer_including_masked_token() {
        // The bearer field is rendered masked but must still round-trip its
        // value back out for the build step.
        let pure = McpForm {
            url: "https://gmail.example/mcp".into(),
            auth: McpAuthKind::Bearer,
            bearer_token: "ya29.secret".into(),
            ..http("gmail")
        };
        let round = FormState::from_pure(pure.clone()).snapshot();
        assert_eq!(round, pure);
        assert_eq!(round.bearer_token, "ya29.secret");
    }

    #[test]
    fn form_state_round_trips_http_oauth() {
        let pure = McpForm {
            url: "https://cal.example/mcp".into(),
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            scopes: "calendar.read calendar.write".into(),
            ..http("cal")
        };
        let round = FormState::from_pure(pure.clone()).snapshot();
        assert_eq!(round, pure);
    }

    #[test]
    fn form_state_blank_defaults_to_stdio_enabled() {
        let f = FormState::blank().snapshot();
        assert_eq!(f.transport, McpTransport::Stdio);
        assert!(f.enabled);
        assert!(!f.editing);
    }

    // --- headless render smoke (draw path, no terminal) -----------------------


    /// Render `state` into an off-screen buffer and return its text content.
    fn rendered(state: &State, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        term.draw(|f| draw(f, state)).expect("draw");
        term.backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// A server whose name / target / detail / configure command all carry
    /// terminal control sequences, to prove the draw path sanitizes them.
    fn hostile_server() -> McpServerView {
        McpServerView {
            name: "evil\u{1b}[2Jbox".into(),
            enabled: true,
            status: "error".into(),
            transport: "http".into(),
            target: "https://h\u{1b}]0;x\u{07}/mcp".into(),
            detail: Some("kaboom\u{1b}c".into()),
            configure_command: vec![
                "adele-daemon".into(),
                "--mcp-oauth-login".into(),
                "cal".into(),
            ],
            ..Default::default()
        }
    }

    fn state_with(servers: Vec<McpServerView>, accounts: Vec<ServiceAccountView>) -> State {
        State {
            servers,
            accounts,
            selected: 0,
            mode: Mode::List,
            form: FormState::blank(),
            error: None,
            busy: None,
            closing: false,
        }
    }

    #[test]
    fn draw_list_sanitizes_hostile_daemon_text() {
        let state = state_with(vec![hostile_server()], Vec::new());
        let text = rendered(&state, 120, 20);
        // The ESC / BEL control bytes must never reach the terminal buffer.
        assert!(!text.contains('\u{1b}'), "ESC reached the buffer");
        assert!(!text.contains('\u{07}'), "BEL reached the buffer");
        // The visible (sanitized) name still renders.
        assert!(text.contains("evil"), "server name missing: {text}");
    }

    #[test]
    fn draw_all_modes_render_without_panicking() {
        let mut state = state_with(
            vec![hostile_server()],
            vec![ServiceAccountView {
                id: "work-google".into(),
                display_name: "Work Google".into(),
                ..Default::default()
            }],
        );
        state.error = Some("something failed".into());
        // List, then the http/oauth edit form, then both overlays — including a
        // cramped terminal (overlay centering must not underflow).
        let _ = rendered(&state, 120, 30);
        state.mode = Mode::Edit;
        state.form = FormState::from_pure(McpForm {
            auth: McpAuthKind::OAuth,
            oauth_account: "work-google".into(),
            ..http("cal")
        });
        let _ = rendered(&state, 120, 30);
        state.mode = Mode::RemoveConfirm;
        let _ = rendered(&state, 30, 8);
        state.mode = Mode::SignInInfo;
        let text = rendered(&state, 120, 30);
        assert!(
            text.contains("--mcp-oauth-login"),
            "sign-in command missing: {text}"
        );
    }

    #[test]
    fn draw_empty_list_shows_add_hint() {
        let state = state_with(Vec::new(), Vec::new());
        let text = rendered(&state, 120, 20);
        assert!(
            text.contains("no MCP servers"),
            "empty hint missing: {text}"
        );
    }
}
