use std::time::Duration;

use super::TelegramConfig;
use crate::alerts::types::{format_metric_label, severity_prefix, AlertMessage};

pub async fn send(cfg: &TelegramConfig, msg: &AlertMessage) -> anyhow::Result<()> {
    let text = format_markdown(msg);
    let url = format!("https://api.telegram.org/bot{}/sendMessage", cfg.bot_token);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let body = serde_json::json!({
        "chat_id": cfg.chat_id,
        "text": text,
        "parse_mode": "Markdown",
        "disable_web_page_preview": true,
    });

    let resp = client.post(&url).json(&body).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Telegram API {status}: {body}"));
    }
    Ok(())
}

fn format_markdown(msg: &AlertMessage) -> String {
    let state_label = if msg.state == "resolved" {
        "✅ *RESOLVED*".to_string()
    } else {
        format!("{} *{}*", severity_prefix(&msg.severity), msg.severity.to_uppercase())
    };

    let metric_label = format_metric_label(&msg.metric);

    let mut lines = vec![format!("{} — {}", state_label, escape_md(&msg.rule_name))];
    if let Some(srv) = &msg.server_label {
        lines.push(format!("Server: {}", escape_md(srv)));
    }
    lines.push(format!("Scope: {}", escape_md(&msg.scope_label)));

    if let (Some(val), Some(thr)) = (msg.value, msg.threshold) {
        let unit = match msg.metric.as_str() {
            "cpu" | "ram" | "disk" | "container_cpu" | "container_ram" => "%",
            "ssl_expiry" => " days",
            _ => "",
        };
        lines.push(format!(
            "{}: {:.2}{} (threshold: {} {:.2}{})",
            metric_label, val, unit, msg.comparison, thr, unit
        ));
    } else if let Some(val) = msg.value {
        lines.push(format!("{metric_label}: {val:.2}"));
    }

    if let Some(ctx) = &msg.context {
        if !ctx.is_empty() {
            lines.push(escape_md(ctx));
        }
    }

    lines.push(format!("Time: {}", msg.time_str));

    lines.join("\n")
}

fn escape_md(s: &str) -> String {
    s.replace('_', "\\_")
        .replace('*', "\\*")
        .replace('`', "\\`")
        .replace('[', "\\[")
}
