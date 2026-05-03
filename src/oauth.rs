//! OAuth2 + PKCE auth flow.
//!
//! Mirrors `adele-gtk/src/oauth.rs`. The daemon advertises its OAuth/OIDC
//! configuration via `GET /auth/config`; we run the standard Authorization
//! Code + PKCE flow with a local loopback redirect, exchange the code for
//! tokens, and hand the access token to `ConnectionConfig::ws_jwt`.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AuthDiscovery {
    #[serde(default)]
    pub methods: Vec<String>,
    pub oidc: Option<OidcDiscovery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OidcDiscovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub scopes: String,
}

#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Discover auth configuration via the daemon's `/auth/config` endpoint.
///
/// Falls back to `methods=[password]` when the server returns a non-success
/// status — keeps backward compatibility with daemons that predate auth
/// discovery.
pub async fn discover_auth_config(ws_url: &str) -> Result<AuthDiscovery> {
    let base_url = ws_url_to_http_base(ws_url);
    let url = format!("{base_url}/auth/config");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("fetching auth config from {url}"))?;

    if !response.status().is_success() {
        return Ok(AuthDiscovery {
            methods: vec!["password".to_string()],
            oidc: None,
        });
    }

    response
        .json::<AuthDiscovery>()
        .await
        .context("parsing auth config response")
}

/// Run the full OAuth2 Authorization Code + PKCE flow.
pub async fn run_oauth_flow(oidc: &OidcDiscovery) -> Result<TokenResponse> {
    use oauth2::{
        AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope,
        TokenResponse as _, TokenUrl, basic::BasicClient,
    };

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client should build");

    let client = BasicClient::new(ClientId::new(oidc.client_id.clone()))
        .set_auth_uri(AuthUrl::new(oidc.authorization_endpoint.clone())?)
        .set_token_uri(TokenUrl::new(oidc.token_endpoint.clone())?);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let redirect_uri = format!("http://127.0.0.1:{}", local_addr.port());

    let client = client.set_redirect_uri(RedirectUrl::new(redirect_uri)?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let scopes: Vec<Scope> = oidc
        .scopes
        .split_whitespace()
        .map(|s| Scope::new(s.to_string()))
        .collect();

    let (auth_url, csrf_state) = {
        let mut req = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(pkce_challenge);
        for scope in &scopes {
            req = req.add_scope(scope.clone());
        }
        req.url()
    };

    open::that(auth_url.to_string()).context("failed to open browser")?;

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        accept_redirect(listener, &csrf_state),
    )
    .await
    .map_err(|_| anyhow::anyhow!("OAuth redirect timed out after 120s"))??;

    let token_result = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("token exchange failed: {e}"))?;

    Ok(TokenResponse {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|t| t.secret().clone()),
    })
}

/// Refresh an access token using a stored refresh token.
///
/// Currently exposed for follow-up work that wires automatic re-auth on
/// 401 responses; not used by the bin entry point yet.
#[allow(dead_code)]
pub async fn refresh_access_token(
    oidc: &OidcDiscovery,
    refresh_token: &str,
) -> Result<TokenResponse> {
    use oauth2::{
        AuthUrl, ClientId, RefreshToken, TokenResponse as _, TokenUrl, basic::BasicClient,
    };

    let http_client = reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client should build");

    let client = BasicClient::new(ClientId::new(oidc.client_id.clone()))
        .set_auth_uri(AuthUrl::new(oidc.authorization_endpoint.clone())?)
        .set_token_uri(TokenUrl::new(oidc.token_endpoint.clone())?);

    let token_result = client
        .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
        .request_async(&http_client)
        .await
        .map_err(|e| anyhow::anyhow!("refresh token exchange failed: {e}"))?;

    Ok(TokenResponse {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|t| t.secret().clone()),
    })
}

async fn accept_redirect(
    listener: tokio::net::TcpListener,
    expected_state: &oauth2::CsrfToken,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut stream, _) = listener.accept().await?;

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP request from redirect"))?;

    let url = url::Url::parse(&format!("http://localhost{path}"))?;
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let state = params
        .get("state")
        .ok_or_else(|| anyhow::anyhow!("missing state parameter"))?;
    if state != expected_state.secret() {
        anyhow::bail!("CSRF state mismatch");
    }

    if let Some(error) = params.get("error") {
        let desc = params.get("error_description").cloned().unwrap_or_default();
        anyhow::bail!("OAuth error: {error} - {desc}");
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow::anyhow!("missing authorization code"))?
        .clone();

    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h2>Login successful!</h2>\
        <p>You can close this window and return to Adele.</p></body></html>";
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;

    Ok(code)
}

fn ws_url_to_http_base(ws_url: &str) -> String {
    let http_url = ws_url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    if let Ok(parsed) = url::Url::parse(&http_url) {
        format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or("localhost"),
            parsed.port().map(|p| format!(":{p}")).unwrap_or_default()
        )
    } else {
        http_url
    }
}

/// True when `methods` advertises OIDC and we have an OIDC config block.
pub fn supports_oauth(discovery: &AuthDiscovery) -> bool {
    discovery.oidc.is_some()
        && discovery
            .methods
            .iter()
            .any(|m| matches!(m.as_str(), "oidc" | "oauth" | "oauth2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_to_http_base_strips_path_and_swaps_scheme() {
        assert_eq!(
            ws_url_to_http_base("ws://127.0.0.1:11339/ws"),
            "http://127.0.0.1:11339"
        );
        assert_eq!(
            ws_url_to_http_base("wss://example.com/ws"),
            "https://example.com"
        );
        assert_eq!(
            ws_url_to_http_base("wss://example.com:8443/ws"),
            "https://example.com:8443"
        );
    }

    #[test]
    fn supports_oauth_true_when_methods_include_oidc_and_oidc_block_present() {
        let d = AuthDiscovery {
            methods: vec!["password".into(), "oidc".into()],
            oidc: Some(OidcDiscovery {
                authorization_endpoint: "https://idp/authorize".into(),
                token_endpoint: "https://idp/token".into(),
                client_id: "abc".into(),
                scopes: "openid profile".into(),
            }),
        };
        assert!(supports_oauth(&d));
    }

    #[test]
    fn supports_oauth_false_without_oidc_block() {
        let d = AuthDiscovery {
            methods: vec!["oidc".into()],
            oidc: None,
        };
        assert!(!supports_oauth(&d));
    }

    #[test]
    fn supports_oauth_false_without_oidc_method() {
        let d = AuthDiscovery {
            methods: vec!["password".into()],
            oidc: Some(OidcDiscovery {
                authorization_endpoint: "https://idp/authorize".into(),
                token_endpoint: "https://idp/token".into(),
                client_id: "abc".into(),
                scopes: "openid".into(),
            }),
        };
        assert!(!supports_oauth(&d));
    }
}
