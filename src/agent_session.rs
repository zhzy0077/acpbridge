//! AgentSession - per-conversation AI agent runtime
//!
//! Each AgentSession is bound to a specific (channel, chat_id, bot_config).
//! It owns an AcpClient and processes incoming messages from its chat.

use crate::acp_client;
use crate::channel::{OutgoingContent, OutgoingMessage};
use crate::config::BotConfig;
use crate::egress::Egress;
use crate::message::{IngressContent, IngressMessage};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

pub struct AgentSessionConfig {
    pub bot_config: BotConfig,
    pub channel_name: String,
    pub chat_id: crate::channel::ChatId,
    pub egress: Arc<Egress>,
}

enum StartupWaitResult {
    Connected,
    Failed,
    TimedOut,
}

pub struct AgentSession {
    config: AgentSessionConfig,
    acp_state: Arc<acp_client::AcpClientState>,
    connected: Arc<tokio::sync::Mutex<bool>>,
    outgoing_rx: Option<mpsc::Receiver<OutgoingMessage>>,
}

impl AgentSession {
    pub fn new(config: AgentSessionConfig) -> Self {
        let working_dir = config
            .bot_config
            .working_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingMessage>(100);

        let acp_state = Arc::new(acp_client::AcpClientState::new(
            working_dir,
            config.bot_config.show_thinking,
            config.bot_config.show_auto_approved,
            outgoing_tx,
            None,
        ));

        Self {
            config,
            acp_state,
            connected: Arc::new(tokio::sync::Mutex::new(false)),
            outgoing_rx: Some(outgoing_rx),
        }
    }

    pub async fn run(mut self, mut event_rx: mpsc::Receiver<IngressMessage>) {
        let chat_id = self.config.chat_id.clone();
        let bot_name = self.config.bot_config.name.clone();
        info!(bot = %bot_name, chat_id = %chat_id.0, "Starting agent session");

        let mut has_sent_instructions = false;

        let mut outgoing_rx = self.outgoing_rx.take().expect("outgoing_rx already taken");
        let egress = self.config.egress.clone();
        let channel_name = self.config.channel_name.clone();
        let chat_id_for_fwd = self.config.chat_id.clone();
        tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                if let Err(e) = egress.send(&channel_name, chat_id_for_fwd.clone(), msg).await {
                    error!(error = %e, "Failed to forward outgoing message");
                }
            }
        });

        self.send_text("Agent is starting...").await;

        let connected_clone = self.connected.clone();
        let startup_failure = Arc::new(tokio::sync::Mutex::new(None));
        let startup_failure_clone = startup_failure.clone();
        let acp_state_clone = self.acp_state.clone();
        let agent_command = self.config.bot_config.agent.command.clone();
        let agent_args = self.config.bot_config.agent.args.clone();

        let acp_handle = tokio::task::spawn_local(async move {
            if let Err(e) = acp_client::run_client(
                agent_command,
                agent_args,
                acp_state_clone.clone(),
                connected_clone.clone(),
            )
            .await
            {
                let error_message = e.to_string();
                error!(error = %error_message, "ACP client error");

                if !*connected_clone.lock().await {
                    *startup_failure_clone.lock().await = Some(error_message.clone());
                    acp_state_clone.send_error(error_message).await;
                }
            }
        });

        match self.wait_for_connection(&acp_handle, &startup_failure).await {
            StartupWaitResult::Connected => {}
            StartupWaitResult::Failed => {
                return;
            }
            StartupWaitResult::TimedOut => {
                self.send_error("Agent failed to connect within timeout").await;
                acp_handle.abort();
                return;
            }
        }

        self.apply_configured_session_settings().await;

        loop {
            match event_rx.recv().await {
                Some(msg) => {
                    if !*self.connected.lock().await {
                        self.send_error("Agent disconnected").await;
                        break;
                    }
                    self.handle_message(msg, &mut has_sent_instructions).await;
                }
                None => {
                    info!(bot = %bot_name, chat_id = %self.config.chat_id.0, "Event channel closed");
                    break;
                }
            }
        }

        info!(bot = %bot_name, chat_id = %self.config.chat_id.0, "Agent session stopping gracefully");

        acp_handle.abort();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        info!(bot = %bot_name, chat_id = %self.config.chat_id.0, "Agent session stopped");
    }

    async fn wait_for_connection(
        &self,
        acp_handle: &JoinHandle<()>,
        startup_failure: &tokio::sync::Mutex<Option<String>>,
    ) -> StartupWaitResult {
        for _ in 0..100 {
            if *self.connected.lock().await {
                return StartupWaitResult::Connected;
            }
            if startup_failure.lock().await.is_some() {
                return StartupWaitResult::Failed;
            }
            if acp_handle.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        if startup_failure.lock().await.is_some() {
            return StartupWaitResult::Failed;
        }

        warn!(chat_id = %self.config.chat_id.0, "Agent connection timeout");
        StartupWaitResult::TimedOut
    }

    async fn apply_configured_session_settings(&self) {
        if let Some(ref model) = self.config.bot_config.model {
            info!(model = %model, "Setting configured model");
            if let Err(e) = acp_client::send_set_model_command(&self.acp_state, model.clone()).await {
                warn!(error = %e, model = %model, "Failed to set configured model");
                self.send_error(&format!("Failed to apply configured model '{}': {}", model, e)).await;
            }
        }

        if let Some(ref mode) = self.config.bot_config.mode {
            info!(mode = %mode, "Setting configured mode");
            if let Err(e) = acp_client::send_set_mode_command(&self.acp_state, mode.clone()).await {
                warn!(error = %e, mode = %mode, "Failed to set configured mode");
                self.send_error(&format!("Failed to apply configured mode '{}': {}", mode, e)).await;
            }
        }

        for (config_id, value) in &self.config.bot_config.config_options {
            info!(config_id = %config_id, value = %value, "Setting configured ACP config option");
            if let Err(e) = acp_client::send_set_config_option_command(
                &self.acp_state,
                config_id.clone(),
                value.clone(),
            )
            .await
            {
                warn!(error = %e, config_id = %config_id, value = %value, "Failed to set configured ACP config option");
                self.send_error(&format!("Failed to apply configured option '{}={}': {}", config_id, value, e)).await;
            }
        }
    }

    fn format_config_options(options: &[acp_client::SessionConfigOptionState]) -> String {
        if options.is_empty() {
            return "No config options available.".to_string();
        }

        let mut lines = vec!["Available config options:".to_string()];
        for option in options {
            let mut header = format!("- {}", option.id);
            if let Some(category) = &option.category {
                header.push_str(&format!(" [{}]", category));
            }
            if let Some(current_value) = &option.current_value {
                header.push_str(&format!(" current={}", current_value));
            }
            header.push_str(&format!(" ({})", option.name));
            lines.push(header);

            if !option.choices.is_empty() {
                let choices = option
                    .choices
                    .iter()
                    .map(|choice| {
                        if let Some(group) = &choice.group {
                            format!("{}/{} ({})", group, choice.value, choice.name)
                        } else {
                            format!("{} ({})", choice.value, choice.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("  choices: {}", choices));
            }
        }

        lines.join("\n")
    }

    async fn handle_message(&self, msg: IngressMessage, has_sent_instructions: &mut bool) {
        match msg.content {
            IngressContent::Text(text) => {
                info!(text = %text, chat_id = %self.config.chat_id.0, "User text message");
                self.handle_text(text, has_sent_instructions).await;
            }
            IngressContent::Command { name, args } => {
                info!(command = %name, args = ?args, chat_id = %self.config.chat_id.0, "User command");
                self.handle_command(&name, args).await;
            }
        }
    }

    async fn handle_text(&self, text: String, has_sent_instructions: &mut bool) {
        let text_to_send = if !*has_sent_instructions {
            if let Some(ref instructions) = self.config.bot_config.instructions {
                *has_sent_instructions = true;
                format!("{}\n\n{}", instructions, text)
            } else {
                *has_sent_instructions = true;
                text
            }
        } else {
            text
        };

        if let Err(e) = acp_client::send_prompt(&self.acp_state, text_to_send).await {
            error!(error = %e, "Failed to send prompt");
        }
    }

    async fn handle_command(&self, name: &str, args: Option<String>) {
        match name {
            "new" => {
                self.send_text("Starting new session...").await;
                match acp_client::send_new_session_command(&self.acp_state).await {
                    Ok(session_id) => {
                        self.apply_configured_session_settings().await;
                        self.send_text(&format!("New session started: {}", session_id)).await;
                    }
                    Err(e) => {
                        self.send_error(&format!("Failed to start new session: {}", e)).await;
                    }
                }
            }
            "model" => {
                if let Some(model_name) = args {
                    self.send_text(&format!("Switching to model: {}...", model_name)).await;
                    match acp_client::send_set_model_command(&self.acp_state, model_name.clone()).await {
                        Ok(_) => {
                            self.send_text(&format!("Model changed to: {}", model_name)).await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to change model: {}", e)).await;
                        }
                    }
                } else {
                    match acp_client::send_get_models_command(&self.acp_state).await {
                        Ok((models, current_model)) if !models.is_empty() => {
                            let mut list = String::new();
                            for m in models {
                                if Some(&m) == current_model.as_ref() {
                                    list.push_str(&format!("- {} (current)\n", m));
                                } else {
                                    list.push_str(&format!("- {}\n", m));
                                }
                            }
                            self.send_text(&format!("Available models:\n{}", list.trim_end())).await;
                        }
                        Ok(_) => {
                            self.send_text("No models available.").await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to get models: {}", e)).await;
                        }
                    }
                }
            }
            "mode" => {
                if let Some(mode_name) = args {
                    self.send_text(&format!("Switching to mode: {}...", mode_name)).await;
                    match acp_client::send_set_mode_command(&self.acp_state, mode_name.clone()).await {
                        Ok(_) => {
                            self.send_text(&format!("Mode changed to: {}", mode_name)).await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to change mode: {}", e)).await;
                        }
                    }
                } else {
                    match acp_client::send_get_modes_command(&self.acp_state).await {
                        Ok((modes, current_mode)) if !modes.is_empty() => {
                            let mut list = String::new();
                            for m in modes {
                                if Some(&m) == current_mode.as_ref() {
                                    list.push_str(&format!("- {} (current)\n", m));
                                } else {
                                    list.push_str(&format!("- {}\n", m));
                                }
                            }
                            self.send_text(&format!("Available modes:\n{}", list.trim_end())).await;
                        }
                        Ok(_) => {
                            self.send_text("No modes available.").await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to get modes: {}", e)).await;
                        }
                    }
                }
            }
            "config" => {
                if let Some(raw_args) = args.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let mut parts = raw_args.split_whitespace();
                    let Some(config_id) = parts.next() else {
                        self.send_error("Usage: /config [id value]").await;
                        return;
                    };
                    let value = parts.collect::<Vec<_>>().join(" ");
                    if value.is_empty() {
                        self.send_error("Usage: /config [id value]").await;
                        return;
                    }

                    self.send_text(&format!("Setting config option {}={}...", config_id, value)).await;
                    match acp_client::send_set_config_option_command(
                        &self.acp_state,
                        config_id.to_string(),
                        value.clone(),
                    )
                    .await
                    {
                        Ok(_) => {
                            self.send_text(&format!("Config option changed: {}={}", config_id, value)).await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to change config option '{}': {}", config_id, e)).await;
                        }
                    }
                } else {
                    match acp_client::send_get_config_options_command(&self.acp_state).await {
                        Ok(options) => {
                            self.send_text(&Self::format_config_options(&options)).await;
                        }
                        Err(e) => {
                            self.send_error(&format!("Failed to get config options: {}", e)).await;
                        }
                    }
                }
            }
            "cd" => {
                if let Some(path) = args {
                    let new_dir = PathBuf::from(&path);
                    if new_dir.exists() && new_dir.is_dir() {
                        self.send_text(&format!("Changing directory to: {}...", new_dir.display())).await;
                        match acp_client::send_change_directory_command(&self.acp_state, new_dir.clone()).await {
                            Ok(session_id) => {
                                self.apply_configured_session_settings().await;
                                self.send_text(&format!("Changed directory to: {}\nNew session: {}", new_dir.display(), session_id)).await;
                            }
                            Err(e) => {
                                self.send_error(&format!("Failed to change directory: {}", e)).await;
                            }
                        }
                    } else {
                        self.send_error(&format!("Directory not found: {}", path)).await;
                    }
                } else {
                    self.send_error("Usage: /cd <path>").await;
                }
            }
            "help" => {
                let help_text = "\
Available commands:
/new - Start a new session
/model [name] - Switch model or list available models
/mode [name] - Switch mode or list available modes
/config [id value] - List config options or set one
/cd <path> - Change working directory
/bot [name] - Switch bot or list available bots
/help - Show this help message";
                self.send_text(help_text).await;
            }
            "bot" => {
                self.send_text("Use /bot to list bots or /bot <name> to switch.").await;
            }
            _ => {
                self.send_error(&format!("Unknown command: /{}", name)).await;
            }
        }
    }

    async fn send_text(&self, text: &str) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Text(text.to_string()),
        };
        if let Err(e) = self.config.egress.send(&self.config.channel_name, self.config.chat_id.clone(), msg).await {
            error!(error = %e, "Failed to send text to user");
        }
    }

    async fn send_error(&self, text: &str) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Error(text.to_string()),
        };
        if let Err(e) = self.config.egress.send(&self.config.channel_name, self.config.chat_id.clone(), msg).await {
            error!(error = %e, "Failed to send error to user");
        }
    }
}
