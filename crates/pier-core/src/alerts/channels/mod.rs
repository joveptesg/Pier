pub mod discord;
pub mod email;
pub mod slack;
pub mod telegram;

use serde::Deserialize;

use super::types::AlertMessage;

#[derive(Debug, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
}

pub async fn send(channel: &str, config_json: &str, msg: &AlertMessage) -> anyhow::Result<()> {
    match channel {
        "telegram" => {
            let cfg: TelegramConfig = serde_json::from_str(config_json)
                .map_err(|e| anyhow::anyhow!("Invalid telegram config: {e}"))?;
            telegram::send(&cfg, msg).await
        }
        "email" => {
            let cfg: email::EmailConfig = serde_json::from_str(config_json)
                .map_err(|e| anyhow::anyhow!("Invalid email config: {e}"))?;
            email::send(&cfg, msg).await
        }
        "discord" => {
            let cfg: discord::DiscordConfig = serde_json::from_str(config_json)
                .map_err(|e| anyhow::anyhow!("Invalid discord config: {e}"))?;
            discord::send(&cfg, msg).await
        }
        "slack" => {
            let cfg: slack::SlackConfig = serde_json::from_str(config_json)
                .map_err(|e| anyhow::anyhow!("Invalid slack config: {e}"))?;
            slack::send(&cfg, msg).await
        }
        other => Err(anyhow::anyhow!("Unsupported channel: {other}")),
    }
}
