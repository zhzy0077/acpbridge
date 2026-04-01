//! Bridge-level message types

use crate::channel::{ChatId, IncomingContent};

#[derive(Debug, Clone)]
pub struct IngressMessage {
    pub channel_name: String,
    pub chat_id: ChatId,
    pub content: IngressContent,
}

#[derive(Debug, Clone)]
pub enum IngressContent {
    Text(String),
    Command { name: String, args: Option<String> },
}

impl From<crate::channel::IncomingMessage> for IngressContent {
    fn from(msg: crate::channel::IncomingMessage) -> Self {
        match msg.content {
            IncomingContent::Text(text) => IngressContent::Text(text),
            IncomingContent::Command { name, args } => IngressContent::Command { name, args },
        }
    }
}
