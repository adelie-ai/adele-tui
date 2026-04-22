use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use api::{WsFrame, WsRequest};
use desktop_assistant_api_model as api;
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::signal::SignalEvent;
use crate::types::{ConversationDetail, ConversationSummary};

type PendingResult = Result<api::CommandResult, String>;

pub struct WsClient {
    outbound_tx: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<PendingResult>>>>,
}

impl WsClient {
    pub async fn connect(
        ws_url: &str,
        bearer_token: &str,
        tls_ca_cert: Option<&Path>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SignalEvent>)> {
        let mut request = ws_url.into_client_request()?;
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            format!("Bearer {bearer_token}").parse()?,
        );

        let connector = if ws_url.starts_with("wss://") {
            Some(build_tls_connector(tls_ca_cert)?)
        } else {
            None
        };

        let (socket, _response) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            None,
            false,
            connector,
        )
        .await?;
        let (mut ws_tx, mut ws_rx) = socket.split();

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                if ws_tx.send(message).await.is_err() {
                    break;
                }
            }
        });

        let pending = Arc::new(Mutex::new(
            HashMap::<String, oneshot::Sender<PendingResult>>::new(),
        ));
        let pending_for_reader = Arc::clone(&pending);

        let (signal_tx, signal_rx) = mpsc::unbounded_channel::<SignalEvent>();
        tokio::spawn(async move {
            while let Some(Ok(message)) = ws_rx.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(frame) = serde_json::from_str::<WsFrame>(&text) else {
                    continue;
                };

                match frame {
                    WsFrame::Result { id, result } => {
                        if let Some(tx) = pending_for_reader.lock().await.remove(&id) {
                            let _ = tx.send(Ok(result));
                        }
                    }
                    WsFrame::Error { id, error } => {
                        if let Some(tx) = pending_for_reader.lock().await.remove(&id) {
                            let _ = tx.send(Err(error));
                        }
                    }
                    WsFrame::Event { event } => {
                        if let Some(signal) = map_event_to_signal(event) {
                            let _ = signal_tx.send(signal);
                        }
                    }
                }
            }

            let _ = signal_tx.send(SignalEvent::Disconnected {
                reason: "WebSocket connection closed".to_string(),
            });

            let mut pending = pending_for_reader.lock().await;
            for (_id, tx) in pending.drain() {
                let _ = tx.send(Err("websocket disconnected".to_string()));
            }
        });

        Ok((
            Self {
                outbound_tx,
                pending,
            },
            signal_rx,
        ))
    }

    async fn send_command(&self, command: api::Command) -> Result<api::CommandResult> {
        let id = uuid::Uuid::new_v4().to_string();
        let request = WsRequest {
            id: id.clone(),
            command,
        };
        let payload = serde_json::to_string(&request)?;

        let (tx, rx) = oneshot::channel::<PendingResult>();
        self.pending.lock().await.insert(id.clone(), tx);

        if self
            .outbound_tx
            .send(Message::Text(payload.into()))
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("failed to send websocket request"));
        }

        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_closed) => Err(anyhow!("websocket response channel closed")),
        }
    }

    pub async fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        let result = self
            .send_command(api::Command::ListConversations {
                max_age_days: None,
                include_archived: false,
            })
            .await?;

        let api::CommandResult::Conversations(items) = result else {
            return Err(anyhow!(
                "unexpected websocket response for list_conversations"
            ));
        };

        Ok(items.into_iter().map(ConversationSummary::from).collect())
    }

    pub async fn list_conversations_with_archived(&self) -> Result<Vec<ConversationSummary>> {
        let result = self
            .send_command(api::Command::ListConversations {
                max_age_days: None,
                include_archived: true,
            })
            .await?;

        let api::CommandResult::Conversations(items) = result else {
            return Err(anyhow!(
                "unexpected websocket response for list_conversations"
            ));
        };

        Ok(items.into_iter().map(ConversationSummary::from).collect())
    }

    pub async fn archive_conversation(&self, id: &str) -> Result<()> {
        let result = self
            .send_command(api::Command::ArchiveConversation { id: id.to_string() })
            .await?;

        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for archive_conversation"
            ));
        };

        Ok(())
    }

    pub async fn unarchive_conversation(&self, id: &str) -> Result<()> {
        let result = self
            .send_command(api::Command::UnarchiveConversation { id: id.to_string() })
            .await?;

        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for unarchive_conversation"
            ));
        };

        Ok(())
    }

    pub async fn get_conversation(&self, id: &str) -> Result<ConversationDetail> {
        let result = self
            .send_command(api::Command::GetConversation { id: id.to_string() })
            .await?;

        let api::CommandResult::Conversation(conversation) = result else {
            return Err(anyhow!(
                "unexpected websocket response for get_conversation"
            ));
        };

        Ok(ConversationDetail::from(conversation))
    }

    pub async fn create_conversation(&self, title: &str) -> Result<String> {
        let result = self
            .send_command(api::Command::CreateConversation {
                title: title.to_string(),
            })
            .await?;

        let api::CommandResult::ConversationId { id } = result else {
            return Err(anyhow!(
                "unexpected websocket response for create_conversation"
            ));
        };
        Ok(id)
    }

    pub async fn delete_conversation(&self, id: &str) -> Result<()> {
        let result = self
            .send_command(api::Command::DeleteConversation { id: id.to_string() })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for delete_conversation"
            ));
        };
        Ok(())
    }

    pub async fn rename_conversation(&self, id: &str, title: &str) -> Result<()> {
        let result = self
            .send_command(api::Command::RenameConversation {
                id: id.to_string(),
                title: title.to_string(),
            })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for rename_conversation"
            ));
        };
        Ok(())
    }

    pub async fn send_prompt(&self, conversation_id: &str, prompt: &str) -> Result<String> {
        self.send_prompt_with_override(conversation_id, prompt, None)
            .await
    }

    /// Send a prompt, optionally pinning it to a specific connection/model/effort.
    ///
    /// The daemon persists the override on the conversation row so subsequent
    /// sends without an override still target the same model (until it is
    /// changed again or the referenced connection goes away).
    pub async fn send_prompt_with_override(
        &self,
        conversation_id: &str,
        prompt: &str,
        override_selection: Option<api::SendPromptOverride>,
    ) -> Result<String> {
        let result = self
            .send_command(api::Command::SendMessage {
                conversation_id: conversation_id.to_string(),
                content: prompt.to_string(),
                override_selection,
            })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!("unexpected websocket response for send_prompt"));
        };

        // WS send-message ack does not include request id; first stream event carries it.
        Ok(String::new())
    }

    // --- Multi-connection API (issue #11 / TUI issue #1) -------------------

    pub async fn list_connections(&self) -> Result<Vec<api::ConnectionView>> {
        let result = self.send_command(api::Command::ListConnections).await?;
        let api::CommandResult::Connections(items) = result else {
            return Err(anyhow!(
                "unexpected websocket response for list_connections"
            ));
        };
        Ok(items)
    }

    pub async fn create_connection(
        &self,
        id: &str,
        config: api::ConnectionConfigView,
    ) -> Result<()> {
        let result = self
            .send_command(api::Command::CreateConnection {
                id: id.to_string(),
                config,
            })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for create_connection"
            ));
        };
        Ok(())
    }

    pub async fn update_connection(
        &self,
        id: &str,
        config: api::ConnectionConfigView,
    ) -> Result<()> {
        let result = self
            .send_command(api::Command::UpdateConnection {
                id: id.to_string(),
                config,
            })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for update_connection"
            ));
        };
        Ok(())
    }

    pub async fn delete_connection(&self, id: &str, force: bool) -> Result<()> {
        let result = self
            .send_command(api::Command::DeleteConnection {
                id: id.to_string(),
                force,
            })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!(
                "unexpected websocket response for delete_connection"
            ));
        };
        Ok(())
    }

    pub async fn list_available_models(
        &self,
        connection_id: Option<&str>,
        refresh: bool,
    ) -> Result<Vec<api::ModelListing>> {
        let result = self
            .send_command(api::Command::ListAvailableModels {
                connection_id: connection_id.map(str::to_string),
                refresh,
            })
            .await?;
        let api::CommandResult::Models(items) = result else {
            return Err(anyhow!(
                "unexpected websocket response for list_available_models"
            ));
        };
        Ok(items)
    }

    pub async fn get_purposes(&self) -> Result<api::PurposesView> {
        let result = self.send_command(api::Command::GetPurposes).await?;
        let api::CommandResult::Purposes(view) = result else {
            return Err(anyhow!("unexpected websocket response for get_purposes"));
        };
        Ok(view)
    }

    pub async fn set_purpose(
        &self,
        purpose: api::PurposeKindApi,
        config: api::PurposeConfigView,
    ) -> Result<()> {
        let result = self
            .send_command(api::Command::SetPurpose { purpose, config })
            .await?;
        let api::CommandResult::Ack = result else {
            return Err(anyhow!("unexpected websocket response for set_purpose"));
        };
        Ok(())
    }
}

fn build_tls_connector(
    ca_cert_path: Option<&Path>,
) -> Result<tokio_tungstenite::Connector> {
    let mut root_store = rustls::RootCertStore::empty();

    if let Some(ca_path) = ca_cert_path {
        let pem_bytes = std::fs::read(ca_path)
            .map_err(|e| anyhow!("reading CA cert {}: {e}", ca_path.display()))?;
        let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
            rustls_pemfile::certs(&mut std::io::BufReader::new(pem_bytes.as_slice()))
                .collect::<std::result::Result<Vec<_>, _>>()?;
        for cert in certs {
            root_store.add(cert)?;
        }
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

pub fn map_event_to_signal(event: api::Event) -> Option<SignalEvent> {
    match event {
        api::Event::AssistantDelta {
            request_id, chunk, ..
        } => Some(SignalEvent::Chunk { request_id, chunk }),
        api::Event::AssistantCompleted {
            request_id,
            full_response,
            ..
        } => Some(SignalEvent::Complete {
            request_id,
            full_response,
        }),
        api::Event::AssistantError {
            request_id, error, ..
        } => Some(SignalEvent::Error { request_id, error }),
        api::Event::ConversationTitleChanged {
            conversation_id,
            title,
        } => Some(SignalEvent::TitleChanged {
            conversation_id,
            title,
        }),
        api::Event::AssistantStatus {
            request_id,
            message,
            ..
        } => Some(SignalEvent::Status {
            request_id,
            message,
        }),
        api::Event::ConversationWarningEmitted {
            conversation_id,
            warning,
        } => Some(SignalEvent::ConversationWarning {
            conversation_id,
            warning,
        }),
        api::Event::ConfigChanged { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stream_events_to_signal_events() {
        let delta = map_event_to_signal(api::Event::AssistantDelta {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            chunk: "he".to_string(),
        });
        assert!(matches!(delta, Some(SignalEvent::Chunk { .. })));

        let complete = map_event_to_signal(api::Event::AssistantCompleted {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            full_response: "hello".to_string(),
        });
        assert!(matches!(complete, Some(SignalEvent::Complete { .. })));

        let error = map_event_to_signal(api::Event::AssistantError {
            conversation_id: "c1".to_string(),
            request_id: "r1".to_string(),
            error: "oops".to_string(),
        });
        assert!(matches!(error, Some(SignalEvent::Error { .. })));
    }

    #[test]
    fn maps_title_changed_event() {
        let event = map_event_to_signal(api::Event::ConversationTitleChanged {
            conversation_id: "c1".to_string(),
            title: "New Title".to_string(),
        });
        assert!(matches!(event, Some(SignalEvent::TitleChanged { .. })));
    }

    #[test]
    fn maps_conversation_warning_event() {
        let event = map_event_to_signal(api::Event::ConversationWarningEmitted {
            conversation_id: "c1".to_string(),
            warning: api::ConversationWarning::DanglingModelSelection {
                previous_selection: api::ConversationModelSelectionView {
                    connection_id: "old".into(),
                    model_id: "gone".into(),
                    effort: None,
                },
                fallback_to: api::ConversationModelSelectionView {
                    connection_id: "work".into(),
                    model_id: "gpt-5".into(),
                    effort: None,
                },
            },
        });
        assert!(matches!(event, Some(SignalEvent::ConversationWarning { .. })));
    }

    #[test]
    fn ignores_config_changed_event() {
        let event = map_event_to_signal(api::Event::ConfigChanged {
            config: api::Config {
                embeddings: api::EmbeddingsSettingsView {
                    connector: "openai".to_string(),
                    model: "text-embedding-3-small".to_string(),
                    base_url: "https://api.openai.com/v1".to_string(),
                    has_api_key: true,
                    available: true,
                    is_default: true,
                },
                persistence: api::PersistenceSettingsView {
                    enabled: false,
                    remote_url: String::new(),
                    remote_name: "origin".to_string(),
                    push_on_update: true,
                },
            },
        });
        assert!(event.is_none());
    }
}
