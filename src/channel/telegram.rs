//! Telegram channel implementation

use crate::channel::{
    Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage,
};
use crate::ingress::Ingress;
use crate::egress::Egress;
use crate::message::{IngressContent, IngressMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, warn};

const MAX_MESSAGE_LENGTH: usize = 4096;
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(500);
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct StreamState {
    msg_id: Option<MessageId>,
    buffer: String,
    last_update: Instant,
}

impl StreamState {
    fn new() -> Self {
        Self {
            msg_id: None,
            buffer: String::new(),
            last_update: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.last_update.elapsed() > STREAM_TIMEOUT
    }
}

#[derive(Debug, Clone)]
pub struct TelegramChannel {
    token: String,
}

impl TelegramChannel {
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

#[async_trait::async_trait]
impl Channel for TelegramChannel {
    async fn start(
        &self,
        ingress: Arc<Ingress>,
        egress: Arc<Egress>,
        channel_name: String,
    ) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);

        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<(ChatId, OutgoingMessage)>(100);
        egress.register_channel(channel_name.clone(), outgoing_tx).await;

        let bot_for_outgoing = bot.clone();
        let outgoing_handle = tokio::spawn(async move {
            let mut stream_states: HashMap<String, StreamState> = HashMap::new();
            let mut timeout_checker = interval(Duration::from_secs(5));
            timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    Some((chat_id, msg)) = outgoing_rx.recv() => {
                        let chat_id_str = chat_id.0.clone();
                        let chat_id_i64: i64 = chat_id_str.parse().unwrap_or(0);
                        let tg_chat_id = teloxide::types::ChatId(chat_id_i64);

                        match msg.content {
                            OutgoingContent::Text(text) => {
                                if let Err(e) = send_text_message(&bot_for_outgoing, tg_chat_id, &text).await {
                                    error!(error = %e, "Failed to send message");
                                }
                            }
                            OutgoingContent::StreamChunk(text) => {
                                if let Err(e) = handle_stream_chunk(
                                    &bot_for_outgoing,
                                    &mut stream_states,
                                    &chat_id_str,
                                    tg_chat_id,
                                    &text,
                                ).await {
                                    error!(error = %e, "Failed to handle stream chunk");
                                }
                            }
                            OutgoingContent::StreamEnd => {
                                if let Err(e) = handle_stream_end(
                                    &bot_for_outgoing,
                                    &mut stream_states,
                                    &chat_id_str,
                                    tg_chat_id,
                                ).await {
                                    error!(error = %e, "Failed to handle stream end");
                                }
                            }
                            OutgoingContent::Error(err) => {
                                if let Err(e) = send_text_message(
                                    &bot_for_outgoing,
                                    tg_chat_id,
                                    &format!("Error: {}", err),
                                ).await {
                                    error!(error = %e, "Failed to send error");
                                }
                            }
                        }
                    }
                    _ = timeout_checker.tick() => {
                        check_and_flush_timeouts(&bot_for_outgoing, &mut stream_states).await;
                    }
                    else => break,
                }
            }
        });

        let ing = ingress.clone();
        let name = channel_name.clone();

        let handler = move |msg: Message, _bot: Bot| {
            let ing = ing.clone();
            let name = name.clone();

            async move {
                let chat_id = ChatId(msg.chat.id.0.to_string());
                if let Some(text) = msg.text() {
                    let incoming = parse_incoming(text);

                    let ingress_msg = IngressMessage {
                        channel_name: name.clone(),
                        chat_id: chat_id.clone(),
                        content: match incoming.content {
                            IncomingContent::Text(text) => IngressContent::Text(text),
                            IncomingContent::Command { name, args } => IngressContent::Command { name, args },
                        },
                    };
                    ing.handle_message(ingress_msg).await;
                }
                ResponseResult::Ok(())
            }
        };

        teloxide::repl(bot, handler).await;
        outgoing_handle.abort();
        Ok(())
    }
}

async fn handle_stream_chunk(
    bot: &Bot,
    stream_states: &mut HashMap<String, StreamState>,
    chat_id_str: &str,
    tg_chat_id: teloxide::types::ChatId,
    text: &str,
) -> anyhow::Result<()> {
    let state = stream_states
        .entry(chat_id_str.to_string())
        .or_insert_with(StreamState::new);

    let new_len = state.buffer.len() + text.len();

    if state.msg_id.is_some() && new_len > MAX_MESSAGE_LENGTH {
        if let Some(msg_id) = state.msg_id {
            bot.edit_message_text(tg_chat_id, msg_id, &state.buffer)
                .await?;
            debug!("Final edit before splitting message");
        }
        state.msg_id = None;
        state.buffer = text.to_string();
        let sent = bot.send_message(tg_chat_id, &state.buffer).await?;
        state.msg_id = Some(sent.id);
        state.last_update = Instant::now();
        debug!(msg_id = ?sent.id, "Started new message after split");
    } else {
        state.buffer.push_str(text);

        if state.msg_id.is_none() {
            let sent = bot.send_message(tg_chat_id, &state.buffer).await?;
            state.msg_id = Some(sent.id);
            state.last_update = Instant::now();
            debug!(msg_id = ?sent.id, "Sent initial stream message");
        } else if state.last_update.elapsed() >= MIN_EDIT_INTERVAL
            && let Some(msg_id) = state.msg_id
        {
            bot.edit_message_text(tg_chat_id, msg_id, &state.buffer)
                .await?;
            state.last_update = Instant::now();
            debug!("Edited stream message");
        }
    }

    Ok(())
}

async fn handle_stream_end(
    bot: &Bot,
    stream_states: &mut HashMap<String, StreamState>,
    chat_id_str: &str,
    tg_chat_id: teloxide::types::ChatId,
) -> anyhow::Result<()> {
    if let Some(state) = stream_states.remove(chat_id_str)
        && let Some(msg_id) = state.msg_id
    {
        bot.edit_message_text(tg_chat_id, msg_id, &state.buffer)
            .await?;
        debug!("Final edit on stream end");
    }

    Ok(())
}

async fn check_and_flush_timeouts(bot: &Bot, stream_states: &mut HashMap<String, StreamState>) {
    let expired_chat_ids: Vec<String> = stream_states
        .iter()
        .filter(|(_, state)| state.is_expired())
        .map(|(chat_id, _)| chat_id.clone())
        .collect();

    for chat_id_str in expired_chat_ids {
        warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");

        let chat_id_i64: i64 = chat_id_str.parse().unwrap_or(0);
        let tg_chat_id = teloxide::types::ChatId(chat_id_i64);

        if let Err(e) = handle_stream_end(bot, stream_states, &chat_id_str, tg_chat_id).await {
            error!(error = %e, chat_id = %chat_id_str, "Failed to flush timeout stream");
        }
    }
}

async fn send_text_message(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    text: &str,
) -> anyhow::Result<()> {
    if text.len() > MAX_MESSAGE_LENGTH {
        for chunk in text.chars().collect::<Vec<_>>().chunks(MAX_MESSAGE_LENGTH) {
            let part = chunk.iter().collect::<String>();
            bot.send_message(chat_id, &part).await?;
        }
    } else {
        bot.send_message(chat_id, text).await?;
    }
    Ok(())
}

fn parse_incoming(text: &str) -> IncomingMessage {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('/') {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let name = parts[0].to_lowercase();
        let args = parts.get(1).map(|s| s.to_string());
        IncomingMessage::command(name, args)
    } else {
        IncomingMessage::text(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_incoming_text() {
        let incoming = parse_incoming("hello");
        match incoming.content {
            IncomingContent::Text(text) => assert_eq!(text, "hello"),
            _ => panic!("Expected text"),
        }
    }

    #[test]
    fn test_parse_incoming_command() {
        let incoming = parse_incoming("/mode fast");
        match incoming.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "mode");
                assert_eq!(args, Some("fast".to_string()));
            }
            _ => panic!("Expected command"),
        }
    }

}
