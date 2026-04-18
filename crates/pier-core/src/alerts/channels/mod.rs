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
        other => Err(anyhow::anyhow!("Unsupported channel: {other}")),
    }
}
