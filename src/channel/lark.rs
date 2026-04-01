//! Lark/Feishu Bot channel implementation
//!
//! Implements WebSocket-based connection to Lark/Feishu Open Platform.
//! Supports p2p (single chat) and Group (@bot) message scenarios.

use crate::channel::{
    Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage,
};
use crate::ingress::Ingress;
use crate::egress::Egress;
use crate::message::{IngressMessage, IngressContent};
use anyhow::{Context, Result, anyhow};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{MissedTickBehavior, interval, sleep};
use tracing::{debug, error, info, warn};

/// Lark Bot channel
#[derive(Debug)]
pub struct LarkChannel {
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
}

impl LarkChannel {
    pub fn new(app_id: String, app_secret: String, base_url: String) -> Self {
        let http_client = HttpClient::new();
        let token_manager = Arc::new(TokenManager::new(
            app_id,
            app_secret,
            base_url,
            http_client.clone(),
        ));
        Self {
            http_client,
            token_manager,
        }
    }
}

#[async_trait::async_trait]
impl Channel for LarkChannel {
    async fn start(
        &self,
        ingress: Arc<Ingress>,
        egress: Arc<Egress>,
        channel_name: String,
    ) -> Result<()> {
        let manager = Arc::new(LarkBotManager::new(
            self.http_client.clone(),
            self.token_manager.clone(),
            channel_name,
            ingress,
            egress,
        ));

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<(ChatId, OutgoingMessage)>(100);
        manager
            .egress
            .register_channel(manager.channel_name.clone(), outgoing_tx)
            .await;

        manager.run(outgoing_rx).await
    }
}

struct LarkBotManager {
    http_client: HttpClient,
    token_manager: Arc<TokenManager>,
    channel_name: String,
    ingress: Arc<Ingress>,
    egress: Arc<Egress>,
}

impl LarkBotManager {
    fn new(
        http_client: HttpClient,
        token_manager: Arc<TokenManager>,
        channel_name: String,
        ingress: Arc<Ingress>,
        egress: Arc<Egress>,
    ) -> Self {
        Self {
            http_client,
            token_manager,
            channel_name,
            ingress,
            egress,
        }
    }

    async fn run(&self, mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>) -> Result<()> {
        let token_manager = self.token_manager.clone();
        let channel_name = self.channel_name.clone();
        let ingress = self.ingress.clone();
        let _egress = self.egress.clone();
        let base_url = self.token_manager.base_url.clone();

        let ws_handle = tokio::spawn(async move {
            let mut reconnect_delay = Duration::from_secs(1);
            const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

            loop {
                match run_websocket_connection(
                    &token_manager,
                    &channel_name,
                    &ingress,
                    &base_url,
                )
                .await
                {
                    Ok(()) => {
                        // This should not happen - run_websocket_connection only returns Ok on programming errors
                        // Treat as error and reconnect
                        error!(
                            "Lark WebSocket returned Ok unexpectedly, reconnecting in {:?}",
                            reconnect_delay
                        );
                        sleep(reconnect_delay).await;
                        reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                    }
                    Err(e) => {
                        error!(error = %e, "Lark WebSocket connection error, reconnecting in {:?}", reconnect_delay);
                        sleep(reconnect_delay).await;
                        reconnect_delay = std::cmp::min(reconnect_delay * 2, MAX_RECONNECT_DELAY);
                    }
                }
            }
        });

        // Handle outgoing messages
        let token_manager = self.token_manager.clone();
        let http_client = self.http_client.clone();

        let outgoing_handle = tokio::spawn(async move {
            // Local state management for stream buffering
            let mut stream_buffers: HashMap<String, (String, std::time::Instant)> = HashMap::new();

            // Timer for checking timeouts (5 seconds for more responsive streaming)
            let mut timeout_checker = interval(Duration::from_secs(5));
            timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // Process new outgoing message
                    Some((chat_id, msg)) = outgoing_rx.recv() => {
                        let chat_id_str = chat_id.0.clone();

                        match msg.content {
                            OutgoingContent::Text(text) => {
                                // Non-streaming message, send directly (with splitting if needed)
                                if let Err(e) = send_long_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &text,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send message");
                                }
                            }
                            OutgoingContent::StreamChunk(text) => {
                                // Buffer mode: accumulate to buffer, update timestamp
                                let now = std::time::Instant::now();
                                let should_flush = {
                                    let entry = stream_buffers
                                        .entry(chat_id_str.clone())
                                        .and_modify(|(buf, ts)| {
                                            buf.push_str(&text);
                                            *ts = now;
                                        })
                                        .or_insert((text.clone(), now));

                                    // Pseudo-streaming: flush if buffer exceeds 500 chars
                                    entry.0.chars().count() >= 500
                                };

                                // Flush immediately if buffer is large enough
                                if should_flush
                                    && let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                                    && !content.is_empty()
                                    && let Err(e) = send_long_message(
                                        &http_client,
                                        &token_manager,
                                        &chat_id,
                                        &content,
                                    )
                                    .await
                                {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send stream chunk");
                                }
                            }
                            OutgoingContent::StreamEnd => {
                                // Stream end: send buffered content and remove from HashMap (avoid memory leak)
                                if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                                    && !content.is_empty()
                                    && let Err(e) = send_long_message(
                                        &http_client,
                                        &token_manager,
                                        &chat_id,
                                        &content,
                                    )
                                    .await
                                {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send stream message");
                                }
                            }
                            OutgoingContent::Error(err) => {
                                let content = format!("Error: {}", err);
                                if let Err(e) = send_long_message(
                                    &http_client,
                                    &token_manager,
                                    &chat_id,
                                    &content,
                                ).await {
                                    error!(error = %e, chat_id = %chat_id_str, "Failed to send error");
                                }
                            }
                        }
                    }
                    // Periodic timeout check (10 seconds for better responsiveness)
                    _ = timeout_checker.tick() => {
                        check_and_flush_timeouts(
                            &http_client,
                            &token_manager,
                            &mut stream_buffers,
                        ).await;
                    }
                    // Exit when channel closed
                    else => break,
                }
            }
        });

        tokio::select! {
            _ = ws_handle => {},
            _ = outgoing_handle => {},
        }

        Ok(())
    }
}

/// Access token manager with automatic refresh and thundering herd protection
#[derive(Debug)]
struct TokenManager {
    app_id: String,
    app_secret: String,
    base_url: String,
    http_client: HttpClient,
    token: RwLock<Option<TokenInfo>>,
    /// Mutex to ensure only one request fetches token at a time
    refresh_lock: tokio::sync::Mutex<()>,
}

#[derive(Clone, Debug)]
struct TokenInfo {
    access_token: String,
    expires_at: SystemTime,
}

impl TokenManager {
    fn new(app_id: String, app_secret: String, base_url: String, http_client: HttpClient) -> Self {
        Self {
            app_id,
            app_secret,
            base_url,
            http_client,
            token: RwLock::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    async fn get_token(&self) -> Result<String> {
        // Fast path: Check if we have a valid cached token (read lock only)
        {
            let token_guard = self.token.read().await;
            if let Some(token) = token_guard.as_ref()
                && SystemTime::now() + Duration::from_secs(300) < token.expires_at
            {
                return Ok(token.access_token.clone());
            }
        }

        // Slow path: Need to fetch new token, acquire exclusive lock
        let _guard = self.refresh_lock.lock().await;

        // Double-check after acquiring lock (another request might have refreshed)
        {
            let token_guard = self.token.read().await;
            if let Some(token) = token_guard.as_ref()
                && SystemTime::now() + Duration::from_secs(300) < token.expires_at
            {
                return Ok(token.access_token.clone());
            }
        }

        // Fetch new token
        self.fetch_token().await
    }

    async fn fetch_token(&self) -> Result<String> {
        let url = format!(
            "{}/open-apis/auth/v3/app_access_token/internal",
            self.base_url
        );
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let response = self
            .http_client
            .post(url)
            .json(&body)
            .send()
            .await
            .context("Failed to send token request")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("Token request failed: {} - {}", status, text));
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        let expires_in: u64 = token_response.expire;
        let expires_at = SystemTime::now() + Duration::from_secs(expires_in);
        let token_info = TokenInfo {
            access_token: token_response.app_access_token.clone(),
            expires_at,
        };

        let mut token_guard = self.token.write().await;
        *token_guard = Some(token_info);

        info!("Successfully obtained Lark app access token");
        Ok(token_response.app_access_token)
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    app_access_token: String,
    expire: u64,
}

/// Event envelope from Lark WebSocket
#[derive(Debug, Deserialize)]
struct EventEnvelope {
    header: EventHeader,
    #[serde(default)]
    event: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct EventHeader {
    #[serde(rename = "event_type")]
    event_type: String,
}

/// Message receive event body
#[derive(Debug, Deserialize)]
struct MessageEvent {
    sender: Sender,
    message: Message,
    #[serde(default)]
    chat: Option<Chat>,
}

#[derive(Debug, Deserialize)]
struct Sender {
    #[serde(rename = "sender_id")]
    sender_id: SenderId,
}

#[derive(Debug, Deserialize)]
struct SenderId {
    #[serde(rename = "open_id")]
    open_id: String,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "message_id")]
    message_id: String,
    #[serde(rename = "message_type")]
    message_type: String,
    content: String,
    #[serde(rename = "chat_type")]
    chat_type: String,
    #[serde(rename = "chat_id")]
    #[serde(default)]
    chat_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    #[serde(rename = "chat_id")]
    chat_id: String,
}

#[derive(Debug, Deserialize)]
struct TextContent {
    text: String,
}

async fn run_websocket_connection(
    token_manager: &Arc<TokenManager>,
    channel_name: &str,
    ingress: &Arc<Ingress>,
    base_url: &str,
) -> Result<()> {
    use openlark_client::Config as LarkConfig;
    use openlark_client::ws_client::{EventDispatcherHandler, LarkWsClient};
    use std::sync::Arc;

    // Build openlark config
    let ws_config = LarkConfig::builder()
        .app_id(token_manager.app_id.clone())
        .app_secret(token_manager.app_secret.clone())
        .base_url(base_url)
        .build()
        .map_err(|e| anyhow!("Failed to build Lark config: {:?}", e))?;

    // Create event channel
    let (payload_tx, mut payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Create event handler and start WebSocket in background
    let event_handler = EventDispatcherHandler::builder()
        .payload_sender(payload_tx)
        .build();

    let ws_config_arc = Arc::new(ws_config);

    // Spawn WebSocket client in background
    let mut ws_client_handle: tokio::task::JoinHandle<
        Result<(), openlark_client::ws_client::WsClientError>,
    > = tokio::spawn(async move { LarkWsClient::open(ws_config_arc, event_handler).await });

    info!("Lark WebSocket client started");

    // Process events
    loop {
        tokio::select! {
            Some(payload) = payload_rx.recv() => {
                debug!(
                    payload = %String::from_utf8_lossy(&payload),
                    bytes = payload.len(),
                    "WebSocket message received"
                );
                if let Err(e) = handle_event(
                    &payload,
                    channel_name,
                    ingress,
                    token_manager,
                ).await {
                    error!(error = %e, "Failed to handle event");
                }
            }
            res = &mut ws_client_handle => {
                // WebSocket task exited - return error to trigger reconnection
                match res {
                    Ok(Ok(())) => {
                        return Err(anyhow!("Lark WebSocket client exited unexpectedly (clean exit)"));
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow!("Lark WebSocket client error: {:?}", e));
                    }
                    Err(e) => {
                        return Err(anyhow!("Lark WebSocket client task panicked: {:?}", e));
                    }
                }
            }
        }
    }
}

fn parse_incoming(content: &str) -> IncomingMessage {
    let content = content.trim();
    if let Some(rest) = content.strip_prefix('/') {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let name = parts[0].to_lowercase();
        let args = parts.get(1).map(|s| s.to_string());
        IncomingMessage::command(name, args)
    } else {
        IncomingMessage::text(content.to_string())
    }
}

fn normalize_content(content: &str) -> String {
    let content = content.trim();

    let mut result = String::new();
    let mut in_tag = false;
    let mut after_at = false;
    let mut just_exited_tag = false;

    for c in content.chars() {
        if c == '<' {
            in_tag = true;
            just_exited_tag = false;
        } else if c == '>' {
            in_tag = false;
            just_exited_tag = true;
        } else if in_tag && c == '@' {
            after_at = true;
        } else if !in_tag {
            if just_exited_tag {
                if c == '@' {
                    after_at = true;
                    just_exited_tag = false;
                    continue;
                }
                just_exited_tag = false;
            }
            if after_at {
                if c.is_whitespace() {
                    after_at = false;
                    continue;
                }
                // Continue skipping mention content
                continue;
            }
            result.push(c);
        }
    }

    result.trim().to_string()
}

/// Send an emoji reaction to a Lark message as an immediate acknowledgement.
/// Best-effort: logs a warning on failure but never propagates the error.
async fn send_reaction(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    message_id: &str,
) -> Result<()> {
    let access_token = token_manager.get_token().await?;

    let url = format!(
        "{}/open-apis/im/v1/messages/{}/reactions",
        token_manager.base_url, message_id
    );

    let body = serde_json::json!({
        "reaction_type": {
            "emoji_type": "THUMBSUP"
        }
    });

    let response = http_client
        .post(&url)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Failed to send reaction request")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Send reaction failed: {} - {}", status, text));
    }

    debug!(message_id = %message_id, "Reaction sent successfully");
    Ok(())
}

async fn handle_event(
    payload: &[u8],
    channel_name: &str,
    ingress: &Arc<Ingress>,
    token_manager: &Arc<TokenManager>,
) -> Result<()> {
    let envelope: EventEnvelope =
        serde_json::from_slice(payload).context("Failed to parse event envelope")?;

    let event_type = envelope.header.event_type;
    debug!(event_type = %event_type, "Received Lark event");

    if event_type != "im.message.receive_v1" {
        debug!(event_type = %event_type, "Ignoring non-message event");
        return Ok(());
    }

    let event_data = envelope
        .event
        .ok_or_else(|| anyhow!("Missing event data"))?;
    let message_event: MessageEvent =
        serde_json::from_value(event_data).context("Failed to parse message event")?;

    if message_event.message.message_type != "text" {
        debug!(msg_type = %message_event.message.message_type, "Ignoring non-text message");
        return Ok(());
    }

    let text_content: TextContent = serde_json::from_str(&message_event.message.content)
        .context("Failed to parse message content")?;

    let chat_id = match message_event.message.chat_type.as_str() {
        "p2p" => {
            ChatId(format!("p2p:{}", message_event.sender.sender_id.open_id))
        }
        "group" => {
            if let Some(chat_id) = message_event.chat.as_ref().map(|c| c.chat_id.clone()) {
                ChatId(format!("group:{}", chat_id))
            } else if let Some(chat_id) = message_event.message.chat_id.clone() {
                ChatId(format!("group:{}", chat_id))
            } else {
                return Err(anyhow!("Missing chat_id for group message"));
            }
        }
        _ => {
            warn!(chat_type = %message_event.message.chat_type, "Unknown chat type");
            return Ok(());
        }
    };

    let content = normalize_content(&text_content.text);

    let tm = Arc::clone(token_manager);
    let message_id = message_event.message.message_id.clone();
    tokio::spawn(async move {
        if let Err(e) = send_reaction(&tm.http_client, &tm, &message_id).await {
            warn!(error = %e, message_id = %message_id, "Failed to send reaction");
        }
    });

    let incoming = parse_incoming(&content);
    let ingress_msg = IngressMessage {
        channel_name: channel_name.to_string(),
        chat_id: chat_id.clone(),
        content: match incoming.content {
            IncomingContent::Text(text) => IngressContent::Text(text),
            IncomingContent::Command { name, args } => IngressContent::Command { name, args },
        },
    };
    ingress.handle_message(ingress_msg).await;

    Ok(())
}

/// Send a message, automatically splitting if it exceeds the max length
async fn send_long_message(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    chat_id: &ChatId,
    content: &str,
) -> Result<()> {
    const MAX_MESSAGE_LENGTH: usize = 4000;

    let char_count = content.chars().count();

    if char_count <= MAX_MESSAGE_LENGTH {
        // Short message - send directly
        send_single_message(http_client, token_manager, chat_id, content).await
    } else {
        // Long message - split into chunks
        let mut start = 0;
        let mut chunk_index = 0;
        let total_chars = char_count;

        while start < total_chars {
            let end = std::cmp::min(start + MAX_MESSAGE_LENGTH, total_chars);

            // Get chunk using char indices
            let chunk: String = content.chars().skip(start).take(end - start).collect();

            // Add part indicator if split into multiple chunks
            let message = if total_chars > MAX_MESSAGE_LENGTH {
                let part_num = chunk_index + 1;
                let total_parts = total_chars.div_ceil(MAX_MESSAGE_LENGTH);
                format!("[Part {}/{}]\n{}", part_num, total_parts, chunk)
            } else {
                chunk
            };

            send_single_message(http_client, token_manager, chat_id, &message).await?;

            start = end;
            chunk_index += 1;

            // Small delay between chunks to avoid rate limiting
            if start < total_chars {
                sleep(Duration::from_millis(200)).await;
            }
        }

        Ok(())
    }
}

/// Send a single message (internal helper)
async fn send_single_message(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    chat_id: &ChatId,
    content: &str,
) -> Result<()> {
    let access_token = token_manager.get_token().await?;

    // Parse chat_id format: "p2p:open_id" or "group:chat_id"
    let (receive_id_type, receive_id) = if let Some(id) = chat_id.0.strip_prefix("p2p:") {
        ("open_id", id)
    } else if let Some(id) = chat_id.0.strip_prefix("group:") {
        ("chat_id", id)
    } else {
        return Err(anyhow!("Invalid chat_id format: {}", chat_id.0));
    };

    // Build request body
    let body = serde_json::json!({
        "receive_id": receive_id,
        "msg_type": "text",
        "content": serde_json::json!({ "text": content }).to_string()
    });

    let url = format!("{}/open-apis/im/v1/messages", token_manager.base_url);

    let response = http_client
        .post(url)
        .query(&[("receive_id_type", receive_id_type)])
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("Failed to send message request")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(anyhow!("Send message failed: {} - {}", status, text));
    }

    debug!("Message sent successfully to {}", chat_id.0);
    Ok(())
}

/// Check and flush timed-out stream buffers (10 second timeout for better responsiveness)
async fn check_and_flush_timeouts(
    http_client: &HttpClient,
    token_manager: &TokenManager,
    stream_buffers: &mut HashMap<String, (String, std::time::Instant)>,
) {
    const STREAM_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_BUFFER_COUNT: usize = 1000; // Memory safety limit
    let now = std::time::Instant::now();

    // Check for timeout
    let expired_chat_ids: Vec<String> = stream_buffers
        .iter()
        .filter(|(_, (_, ts))| now.duration_since(*ts) > STREAM_TIMEOUT)
        .map(|(chat_id, _)| chat_id.clone())
        .collect();

    for chat_id_str in expired_chat_ids {
        warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");

        if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
            && !content.is_empty()
        {
            let chat_id = ChatId(chat_id_str.clone());
            if let Err(e) = send_long_message(http_client, token_manager, &chat_id, &content).await
            {
                error!(error = %e, chat_id = %chat_id_str, "Failed to send timeout stream message");
            }
        }
    }

    // Memory safety: if too many buffers, flush the oldest ones
    if stream_buffers.len() > MAX_BUFFER_COUNT {
        let mut entries: Vec<(String, std::time::Instant)> = stream_buffers
            .iter()
            .map(|(k, (_, ts))| (k.clone(), *ts))
            .collect();
        entries.sort_by(|a, b| a.1.cmp(&b.1));

        let to_remove = stream_buffers.len() - MAX_BUFFER_COUNT;
        for (chat_id_str, _) in entries.into_iter().take(to_remove) {
            warn!(chat_id = %chat_id_str, "Too many stream buffers, flushing oldest");

            if let Some((content, _)) = stream_buffers.remove(&chat_id_str)
                && !content.is_empty()
            {
                let chat_id = ChatId(chat_id_str.clone());
                if let Err(e) =
                    send_long_message(http_client, token_manager, &chat_id, &content).await
                {
                    error!(error = %e, chat_id = %chat_id_str, "Failed to send buffer overflow message");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_id_parsing() {
        let p2p_id = ChatId("p2p:test_open_id".to_string());
        assert!(p2p_id.0.starts_with("p2p:"));

        let group_id = ChatId("group:test_chat_id".to_string());
        assert!(group_id.0.starts_with("group:"));
    }

    #[test]
    fn test_normalize_content_plain_text() {
        let content = normalize_content("Hello world");
        assert_eq!(content, "Hello world");
    }

    #[test]
    fn test_normalize_content_with_command() {
        let content = normalize_content("/help");
        assert_eq!(content, "/help");
    }

    #[test]
    fn test_normalize_content_strips_lark_mention() {
        let content = normalize_content("<at user_id=\"ou_123abc\">@Bot</at> /help");
        assert_eq!(content, "/help");
    }

    #[test]
    fn test_normalize_content_strips_legacy_mention() {
        let content = normalize_content("<at user_id=\"ou_xxx\">@Bot</at> Hello");
        assert_eq!(content, "Hello");
    }

    #[test]
    fn test_normalize_content_at_format_with_args() {
        let content = normalize_content("<at user_id=\"ou_xxx\">@Bot</at> /model gpt-4");
        assert_eq!(content, "/model gpt-4");
    }

}
