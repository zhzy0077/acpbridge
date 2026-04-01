//! Egress - outgoing message routing from AgentSession to Channels
//!
//! Egress receives outgoing messages from AgentSessions and routes them
//! to the appropriate channel based on channel_name.

use crate::channel::{ChatId, OutgoingMessage};
use std::collections::HashMap;
use tokio::sync::{Mutex, mpsc};

pub struct Egress {
    channel_txs: Mutex<HashMap<String, mpsc::Sender<(ChatId, OutgoingMessage)>>>,
}

impl Egress {
    pub fn new() -> Self {
        Self {
            channel_txs: Mutex::new(HashMap::new()),
        }
    }

    pub async fn register_channel(
        &self,
        channel_name: String,
        tx: mpsc::Sender<(ChatId, OutgoingMessage)>,
    ) {
        self.channel_txs.lock().await.insert(channel_name, tx);
    }

    pub async fn send(
        &self,
        channel_name: &str,
        chat_id: ChatId,
        msg: OutgoingMessage,
    ) -> anyhow::Result<()> {
        let txs = self.channel_txs.lock().await;
        let tx = txs
            .get(channel_name)
            .ok_or_else(|| anyhow::anyhow!("Channel not found: {}", channel_name))?;
        tx.send((chat_id, msg))
            .await
            .map_err(|_| anyhow::anyhow!("Channel outgoing channel closed"))?;
        Ok(())
    }
}

impl Default for Egress {
    fn default() -> Self {
        Self::new()
    }
}
