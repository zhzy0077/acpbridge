//! ACP Connector - Bridge between messaging platforms and ACP Agent CLI

mod acp_client;
mod agent_session;
mod channel;
mod config;
mod egress;
mod ingress;
mod message;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let config_path = std::env::var("ACP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.yaml"));

    info!(path = %config_path.display(), "Loading configuration");
    let config = config::Config::from_file(&config_path)?;

    if config.channels.is_empty() {
        error!("No channels configured.");
        std::process::exit(1);
    }

    info!(
        bot_count = config.bots.len(),
        channel_count = config.channels.len(),
        "Starting ACP Connector"
    );

    let egress = Arc::new(egress::Egress::new());
    let ingress = Arc::new(ingress::Ingress::new(config.clone(), egress.clone()));

    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async {
            let mut handles = Vec::new();

            for channel_config in &config.channels {
                let channel_name = channel_config.name.clone();

                let channel: Arc<dyn channel::Channel> = match &channel_config.platform {
                    config::PlatformConfig::Telegram { telegram } => {
                        Arc::new(channel::TelegramChannel::new(telegram.token.clone()))
                    }
                    config::PlatformConfig::Qq { qq } => Arc::new(channel::QqChannel::new(
                        qq.app_id.clone(),
                        qq.client_secret.clone(),
                    )),
                    config::PlatformConfig::Lark { lark } => Arc::new(channel::LarkChannel::new(
                        lark.app_id.clone(),
                        lark.app_secret.clone(),
                        lark.base_url.clone(),
                    )),
                    config::PlatformConfig::Wechat { wechat } => {
                        let wechat_config = channel::WeChatConfig::new(
                            wechat.bot_token.clone(),
                            Some(wechat.base_url.clone()),
                        );
                        Arc::new(channel::WeChatChannel::new(wechat_config))
                    }
                };

                let ing = ingress.clone();
                let eg = egress.clone();
                let name = channel_name.clone();

                let handle = tokio::spawn(async move {
                    if let Err(e) = channel.start(ing, eg, name).await {
                        error!(error = %e, channel = %channel_name, "Channel error");
                    }
                });
                handles.push(handle);
            }

            let ingress_cleanup = ingress.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    ingress_cleanup
                        .cleanup_idle(std::time::Duration::from_secs(30 * 60))
                        .await;
                }
            });

            for handle in handles {
                if let Err(e) = handle.await {
                    error!(error = %e, "Channel task panicked");
                }
            }

            Ok(())
        })
        .await
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,acpconnector=debug"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}
