use anyhow::Result;
use async_trait::async_trait;
use desktop_assistant_api_model as api;
use tokio::sync::mpsc;

use crate::auth::resolve_ws_bearer_token;
use crate::config::{ConnectionConfig, TransportMode};
use crate::signal::SignalEvent;
use crate::types::{ConversationDetail, ConversationSummary};
use crate::ws_client::WsClient;

#[async_trait]
pub trait AssistantClient: Send + Sync {
    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>>;
    async fn list_conversations_with_archived(&self) -> Result<Vec<ConversationSummary>>;
    async fn get_conversation(&self, id: &str) -> Result<ConversationDetail>;
    async fn create_conversation(&self, title: &str) -> Result<String>;
    async fn delete_conversation(&self, id: &str) -> Result<()>;
    async fn rename_conversation(&self, id: &str, title: &str) -> Result<()>;
    async fn archive_conversation(&self, id: &str) -> Result<()>;
    async fn unarchive_conversation(&self, id: &str) -> Result<()>;
    async fn send_prompt(&self, conversation_id: &str, prompt: &str) -> Result<String>;
    async fn send_prompt_with_override(
        &self,
        conversation_id: &str,
        prompt: &str,
        override_selection: Option<api::SendPromptOverride>,
    ) -> Result<String>;

    // Named-connection / purposes / models API (issue #11). Only the WS
    // transport implements these today — the D-Bus adapter predates the
    // multi-connection API surface and still speaks the legacy commands.
    async fn list_connections(&self) -> Result<Vec<api::ConnectionView>>;
    async fn create_connection(&self, id: &str, config: api::ConnectionConfigView) -> Result<()>;
    async fn update_connection(&self, id: &str, config: api::ConnectionConfigView) -> Result<()>;
    async fn delete_connection(&self, id: &str, force: bool) -> Result<()>;
    async fn list_available_models(
        &self,
        connection_id: Option<&str>,
        refresh: bool,
    ) -> Result<Vec<api::ModelListing>>;
    async fn get_purposes(&self) -> Result<api::PurposesView>;
    async fn set_purpose(
        &self,
        purpose: api::PurposeKindApi,
        config: api::PurposeConfigView,
    ) -> Result<()>;
}

pub enum TransportClient {
    #[cfg(feature = "dbus")]
    Dbus(crate::dbus_client::DbusClient),
    Ws(WsClient),
}

fn multi_connection_unsupported<T>(op: &str) -> Result<T> {
    Err(anyhow::anyhow!(
        "{op} is not supported over D-Bus — connect with --transport=ws to manage connections"
    ))
}

#[async_trait]
impl AssistantClient for TransportClient {
    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.list_conversations().await,
            Self::Ws(client) => client.list_conversations().await,
        }
    }

    async fn list_conversations_with_archived(&self) -> Result<Vec<ConversationSummary>> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.list_conversations_with_archived().await,
            Self::Ws(client) => client.list_conversations_with_archived().await,
        }
    }

    async fn get_conversation(&self, id: &str) -> Result<ConversationDetail> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.get_conversation(id).await,
            Self::Ws(client) => client.get_conversation(id).await,
        }
    }

    async fn create_conversation(&self, title: &str) -> Result<String> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.create_conversation(title).await,
            Self::Ws(client) => client.create_conversation(title).await,
        }
    }

    async fn delete_conversation(&self, id: &str) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.delete_conversation(id).await,
            Self::Ws(client) => client.delete_conversation(id).await,
        }
    }

    async fn rename_conversation(&self, id: &str, title: &str) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.rename_conversation(id, title).await,
            Self::Ws(client) => client.rename_conversation(id, title).await,
        }
    }

    async fn archive_conversation(&self, id: &str) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.archive_conversation(id).await,
            Self::Ws(client) => client.archive_conversation(id).await,
        }
    }

    async fn unarchive_conversation(&self, id: &str) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.unarchive_conversation(id).await,
            Self::Ws(client) => client.unarchive_conversation(id).await,
        }
    }

    async fn send_prompt(&self, conversation_id: &str, prompt: &str) -> Result<String> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => client.send_prompt(conversation_id, prompt).await,
            Self::Ws(client) => client.send_prompt(conversation_id, prompt).await,
        }
    }

    async fn send_prompt_with_override(
        &self,
        conversation_id: &str,
        prompt: &str,
        override_selection: Option<api::SendPromptOverride>,
    ) -> Result<String> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(client) => {
                // D-Bus does not plumb the override today; fall back to the
                // un-overridden send so the message still reaches the daemon.
                let _ = override_selection;
                client.send_prompt(conversation_id, prompt).await
            }
            Self::Ws(client) => {
                client
                    .send_prompt_with_override(conversation_id, prompt, override_selection)
                    .await
            }
        }
    }

    async fn list_connections(&self) -> Result<Vec<api::ConnectionView>> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("list_connections"),
            Self::Ws(client) => client.list_connections().await,
        }
    }

    async fn create_connection(&self, id: &str, config: api::ConnectionConfigView) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("create_connection"),
            Self::Ws(client) => client.create_connection(id, config).await,
        }
    }

    async fn update_connection(&self, id: &str, config: api::ConnectionConfigView) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("update_connection"),
            Self::Ws(client) => client.update_connection(id, config).await,
        }
    }

    async fn delete_connection(&self, id: &str, force: bool) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("delete_connection"),
            Self::Ws(client) => client.delete_connection(id, force).await,
        }
    }

    async fn list_available_models(
        &self,
        connection_id: Option<&str>,
        refresh: bool,
    ) -> Result<Vec<api::ModelListing>> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("list_available_models"),
            Self::Ws(client) => client.list_available_models(connection_id, refresh).await,
        }
    }

    async fn get_purposes(&self) -> Result<api::PurposesView> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("get_purposes"),
            Self::Ws(client) => client.get_purposes().await,
        }
    }

    async fn set_purpose(
        &self,
        purpose: api::PurposeKindApi,
        config: api::PurposeConfigView,
    ) -> Result<()> {
        match self {
            #[cfg(feature = "dbus")]
            Self::Dbus(_) => multi_connection_unsupported("set_purpose"),
            Self::Ws(client) => client.set_purpose(purpose, config).await,
        }
    }
}

pub fn transport_label(config: &ConnectionConfig) -> String {
    match config.transport_mode {
        TransportMode::Dbus => "Connected via D-Bus".to_string(),
        TransportMode::Ws => format!("Connected to {}", config.ws_url),
    }
}

pub async fn connect_transport(
    config: &ConnectionConfig,
) -> Result<(TransportClient, mpsc::UnboundedReceiver<SignalEvent>)> {
    match config.transport_mode {
        #[cfg(feature = "dbus")]
        TransportMode::Dbus => {
            let client = crate::dbus_client::DbusClient::connect().await?;
            let signal_rx = client.subscribe_signals().await?;
            Ok((TransportClient::Dbus(client), signal_rx))
        }
        #[cfg(not(feature = "dbus"))]
        TransportMode::Dbus => Err(anyhow::anyhow!(
            "D-Bus transport is not available (compiled without dbus feature)"
        )),
        TransportMode::Ws => {
            let token = resolve_ws_bearer_token(config).await?;
            let (client, signal_rx) =
                WsClient::connect(&config.ws_url, &token, config.tls_ca_cert.as_deref()).await?;
            Ok((TransportClient::Ws(client), signal_rx))
        }
    }
}
