use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::alerts::types::{format_metric_label, AlertMessage};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiscordConfig {
    pub webhook_url: String,
    /// When true, prepend `@here` so the message notifies the channel.
    pub ping: bool,
}

pub async fn send(cfg: &DiscordConfig, msg: &AlertMessage) -> anyhow::Result<()> {
    if cfg.webhook_url.is_empty() {
        return Err(anyhow::anyhow!("Discord webhook_url is required"));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let body = build_payload(cfg, msg);
    let resp = client.post(&cfg.webhook_url).json(&body).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Discord webhook {status}: {err_body}"));
    }
    Ok(())
}

fn build_payload(cfg: &DiscordConfig, msg: &AlertMessage) -> serde_json::Value {
    // Embed colour by severity (Discord decimal)
    let color: u32 = if msg.state == "resolved" {
        0x2ECC71
    } else {
        match msg.severity.as_str() {
            "critical" => 0xE74C3C,
            "warning" => 0xF39C12,
            _ => 0x3498DB,
        }
    };

    let title = if msg.state == "resolved" {
        format!("✅ Resolved — {}", msg.rule_name)
    } else {
        let prefix = match msg.severity.as_str() {
            "critical" => "🚨",
            "warning" => "⚠️",
            _ => "ℹ️",
        };
        format!(
            "{prefix} {} — {}",
            msg.severity.to_uppercase(),
            msg.rule_name
        )
    };

    let mut fields: Vec<serde_json::Value> = Vec::new();
    if let Some(srv) = &msg.server_label {
        fields.push(serde_json::json!({ "name": "Server", "value": srv, "inline": true }));
    }
    fields.push(serde_json::json!({
        "name": "Scope",
        "value": msg.scope_label,
        "inline": true,
    }));

    let unit = match msg.metric.as_str() {
        "cpu" | "ram" | "disk" | "container_cpu" | "container_ram" => "%",
        "ssl_expiry" => " days",
        _ => "",
    };
    let label = format_metric_label(&msg.metric);
    if let (Some(val), Some(thr)) = (msg.value, msg.threshold) {
        fields.push(serde_json::json!({
            "name": label,
            "value": format!("{val:.2}{unit} (threshold: {} {thr:.2}{unit})", msg.comparison),
            "inline": false,
        }));
    } else if let Some(val) = msg.value {
        fields.push(serde_json::json!({
            "name": label,
            "value": format!("{val:.2}{unit}"),
            "inline": false,
        }));
    }

    let description = msg.context.clone().unwrap_or_default();

    let content = if cfg.ping && msg.state != "resolved" {
        "@here".to_string()
    } else {
        String::new()
    };

    serde_json::json!({
        "username": "Pier",
        "content": content,
        "embeds": [{
            "title": title,
            "description": description,
            "color": color,
            "fields": fields,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }]
    })
}
