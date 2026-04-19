use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::alerts::types::{format_metric_label, AlertMessage};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SlackConfig {
    pub webhook_url: String,
}

pub async fn send(cfg: &SlackConfig, msg: &AlertMessage) -> anyhow::Result<()> {
    if cfg.webhook_url.is_empty() {
        return Err(anyhow::anyhow!("Slack webhook_url is required"));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let body = build_payload(msg);
    let resp = client.post(&cfg.webhook_url).json(&body).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Slack webhook {status}: {err_body}"));
    }
    Ok(())
}

fn build_payload(msg: &AlertMessage) -> serde_json::Value {
    // Slack attachment color uses hex strings (with #).
    let color = if msg.state == "resolved" {
        "#2ECC71"
    } else {
        match msg.severity.as_str() {
            "critical" => "#E74C3C",
            "warning" => "#F39C12",
            _ => "#3498DB",
        }
    };

    let title = if msg.state == "resolved" {
        format!(":white_check_mark: Resolved — {}", msg.rule_name)
    } else {
        let prefix = match msg.severity.as_str() {
            "critical" => ":rotating_light:",
            "warning" => ":warning:",
            _ => ":information_source:",
        };
        format!(
            "{prefix} {} — {}",
            msg.severity.to_uppercase(),
            msg.rule_name
        )
    };

    let mut fields: Vec<serde_json::Value> = Vec::new();
    if let Some(srv) = &msg.server_label {
        fields.push(serde_json::json!({ "title": "Server", "value": srv, "short": true }));
    }
    fields.push(serde_json::json!({
        "title": "Scope", "value": msg.scope_label, "short": true,
    }));

    let unit = match msg.metric.as_str() {
        "cpu" | "ram" | "disk" | "container_cpu" | "container_ram" => "%",
        "ssl_expiry" => " days",
        _ => "",
    };
    let label = format_metric_label(&msg.metric);
    if let (Some(val), Some(thr)) = (msg.value, msg.threshold) {
        fields.push(serde_json::json!({
            "title": label,
            "value": format!("{val:.2}{unit} (threshold: {} {thr:.2}{unit})", msg.comparison),
            "short": false,
        }));
    } else if let Some(val) = msg.value {
        fields.push(serde_json::json!({
            "title": label,
            "value": format!("{val:.2}{unit}"),
            "short": false,
        }));
    }

    let text = msg.context.clone().unwrap_or_default();

    serde_json::json!({
        "attachments": [{
            "color": color,
            "title": title,
            "text": text,
            "fields": fields,
            "footer": "Pier",
            "ts": chrono::Utc::now().timestamp(),
        }]
    })
}
