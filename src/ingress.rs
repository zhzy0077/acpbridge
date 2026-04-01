//! Ingress - incoming message routing from Channels to AgentSessions
//!
//! Ingress is the entry point for all incoming messages from channels.
//! It routes messages to the appropriate AgentSession and handles
//! session lifecycle (creation, switching, cleanup).

use crate::agent_session::{AgentSession, AgentSessionConfig};
use crate::channel::{ChatId, OutgoingContent, OutgoingMessage};
use crate::config::Config;
use crate::egress::Egress;
use crate::message::{IngressContent, IngressMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

struct RunningSession {
    last_active: Instant,
}

type SessionKey = (String, ChatId, String);
type SessionSender = mpsc::Sender<IngressMessage>;

impl Clone for RunningSession {
    fn clone(&self) -> Self {
        RunningSession {
            last_active: self.last_active,
        }
    }
}

pub struct Ingress {
    config: Config,
    egress: Arc<Egress>,
    active_sessions: Arc<Mutex<HashMap<(String, ChatId), String>>>,
    running_sessions: Arc<Mutex<HashMap<SessionKey, RunningSession>>>,
    session_txs: Arc<Mutex<HashMap<SessionKey, SessionSender>>>,
}

impl Ingress {
    pub fn new(config: Config, egress: Arc<Egress>) -> Self {
        Self {
            config,
            egress,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            running_sessions: Arc::new(Mutex::new(HashMap::new())),
            session_txs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn handle_message(&self, msg: IngressMessage) {
        let channel_name = msg.channel_name.clone();
        let chat_id = msg.chat_id.clone();

        if let IngressContent::Command { name, args } = &msg.content
            && name == "bot"
        {
            self.handle_bot_command(&channel_name, &chat_id, args.clone()).await;
            return;
        }

        let active_bot = self.active_sessions.lock().await.get(&(channel_name.clone(), chat_id.clone())).cloned();

        let bot_name = match active_bot {
            Some(bot_name) => {
                let mut sessions = self.running_sessions.lock().await;
                if let Some(rb) = sessions.get_mut(&(channel_name.clone(), chat_id.clone(), bot_name.clone())) {
                    rb.last_active = Instant::now();
                }
                bot_name
            }
            None => {
                match self.get_or_create_session(&channel_name, &chat_id).await {
                    Some(name) => name,
                    None => {
                        let _ = self.egress.send(
                            &channel_name,
                            chat_id.clone(),
                            OutgoingMessage {
                                content: OutgoingContent::Error("Failed to create session".to_string()),
                            },
                        ).await;
                        return;
                    }
                }
            }
        };

        let tx_key = (channel_name.clone(), chat_id.clone(), bot_name.clone());
        if let Some(tx) = self.session_txs.lock().await.get(&tx_key)
            && let Err(e) = tx.send(msg).await
        {
            warn!(error = %e, "Session receiver dropped");
        }
    }

    async fn get_or_create_session(&self, channel_name: &str, chat_id: &ChatId) -> Option<String> {
        let channel_config = self.config.channels.iter().find(|c| c.name == channel_name)?;

        let bot_name = channel_config
            .default_bot
            .clone()
            .or_else(|| channel_config.bots.first().cloned())?;

        self.spawn_session(channel_name, chat_id, &bot_name)
            .await
            .ok()?;

        Some(bot_name)
    }

    async fn spawn_session(
        &self,
        channel_name: &str,
        chat_id: &ChatId,
        bot_name: &str,
    ) -> anyhow::Result<()> {
        let bot_config = self
            .config
            .get_bot(bot_name)
            .ok_or_else(|| anyhow::anyhow!("Bot config not found: {}", bot_name))?
            .clone();

        info!(
            bot = %bot_name,
            channel = %channel_name,
            chat_id = %chat_id.0,
            "Spawning agent session"
        );

        let (event_tx, event_rx) = mpsc::channel::<IngressMessage>(100);

        let session_config = AgentSessionConfig {
            bot_config: bot_config.clone(),
            channel_name: channel_name.to_string(),
            chat_id: chat_id.clone(),
            egress: self.egress.clone(),
        };

        let session = AgentSession::new(session_config);

        self.active_sessions.lock().await.insert(
            (channel_name.to_string(), chat_id.clone()),
            bot_name.to_string(),
        );
        self.running_sessions.lock().await.insert(
            (channel_name.to_string(), chat_id.clone(), bot_name.to_string()),
            RunningSession { last_active: Instant::now() },
        );
        self.session_txs.lock().await.insert(
            (channel_name.to_string(), chat_id.clone(), bot_name.to_string()),
            event_tx,
        );

        let channel_name_clone = channel_name.to_string();
        let chat_id_clone = chat_id.clone();
        let bot_name_clone = bot_name.to_string();
        let running_sessions = self.running_sessions.clone();
        let active_sessions = self.active_sessions.clone();
        let session_txs = self.session_txs.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build tokio runtime for agent session");
            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                session.run(event_rx).await;
                let mut rs = running_sessions.lock().await;
                let mut as_ = active_sessions.lock().await;
                let mut stx = session_txs.lock().await;

                rs.remove(&(channel_name_clone.clone(), chat_id_clone.clone(), bot_name_clone.clone()));
                as_.remove(&(channel_name_clone.clone(), chat_id_clone.clone()));
                stx.remove(&(channel_name_clone.clone(), chat_id_clone.clone(), bot_name_clone.clone()));

                info!(
                    bot = %bot_name_clone,
                    channel = %channel_name_clone,
                    chat_id = %chat_id_clone.0,
                    "Agent session exited"
                );
            }));
        });

        Ok(())
    }

    async fn handle_bot_command(&self, channel_name: &str, chat_id: &ChatId, args: Option<String>) {
        let channel_config = match self.config.channels.iter().find(|c| c.name == channel_name) {
            Some(c) => c,
            None => return,
        };

        let current_bot = self
            .active_sessions
            .lock()
            .await
            .get(&(channel_name.to_string(), chat_id.clone()))
            .cloned();

        match args {
            None => {
                let mut response = "Available bots:\n".to_string();
                for bot_name in &channel_config.bots {
                    let marker = if current_bot.as_ref() == Some(bot_name) {
                        " (current)"
                    } else {
                        ""
                    };
                    let desc = self
                        .config
                        .get_bot(bot_name)
                        .and_then(|b| b.description.as_deref())
                        .unwrap_or("No description");
                    response.push_str(&format!("- {}{} — {}\n", bot_name, marker, desc));
                }
                let _ = self.egress.send(
                    channel_name,
                    chat_id.clone(),
                    OutgoingMessage {
                        content: OutgoingContent::Text(response.trim().to_string()),
                    },
                ).await;
            }
            Some(target_name) => {
                let target_name = target_name.trim().to_string();

                if !channel_config.bots.contains(&target_name) {
                    let _ = self.egress.send(
                        channel_name,
                        chat_id.clone(),
                        OutgoingMessage {
                            content: OutgoingContent::Error(format!(
                                "Bot '{}' is not available on this channel. Use /bot to list.",
                                target_name
                            )),
                        },
                    ).await;
                    return;
                }

                if current_bot.as_ref() == Some(&target_name) {
                    let _ = self.egress.send(
                        channel_name,
                        chat_id.clone(),
                        OutgoingMessage {
                            content: OutgoingContent::Text(format!("Already using bot: {}", target_name)),
                        },
                    ).await;
                    return;
                }

                if let Some(ref old_name) = current_bot {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    info!(bot = %old_name, "Stopped old agent session");
                }

                let _ = self.egress.send(
                    channel_name,
                    chat_id.clone(),
                    OutgoingMessage {
                        content: OutgoingContent::Text(format!("Switching to bot: {}...", target_name)),
                    },
                ).await;

                match self.spawn_session(channel_name, chat_id, &target_name).await {
                    Ok(()) => {
                        let _ = self.egress.send(
                            channel_name,
                            chat_id.clone(),
                            OutgoingMessage {
                                content: OutgoingContent::Text(format!("Switched to bot: {}", target_name)),
                            },
                        ).await;
                    }
                    Err(e) => {
                        let _ = self.egress.send(
                            channel_name,
                            chat_id.clone(),
                            OutgoingMessage {
                                content: OutgoingContent::Error(format!("Failed to start bot: {}", e)),
                            },
                        ).await;
                    }
                }
            }
        }
    }

    pub async fn cleanup_idle(&self, timeout: std::time::Duration) {
        let now = Instant::now();
        let mut active = self.active_sessions.lock().await;
        let mut running = self.running_sessions.lock().await;
        let mut session_txs = self.session_txs.lock().await;

        let expired: Vec<(String, ChatId, String)> = running
            .iter()
            .filter(|(_, rb)| now.duration_since(rb.last_active) > timeout)
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired {
            if running.remove(&key).is_some() {
                session_txs.remove(&key);
                active.remove(&(key.0.clone(), key.1.clone()));
                info!(
                    bot = %key.2,
                    channel = %key.0,
                    chat_id = %key.1.0,
                    "Cleaned up idle agent session"
                );
            }
        }
    }
}
