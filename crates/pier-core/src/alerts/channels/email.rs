use std::time::Duration;

use lettre::{
    message::{header::ContentType, Mailbox, Message},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
    AsyncSmtpTransport, AsyncTransport, Tokio1Executor,
};
use serde::{Deserialize, Serialize};

use crate::alerts::types::{format_metric_label, severity_prefix, AlertMessage};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    pub driver: String, // "smtp" | "brevo" | "resend"
    pub from_name: String,
    pub from_address: String,
    pub to_address: String,
    pub smtp: SmtpConfig,
    pub brevo: ApiKeyConfig,
    pub resend: ApiKeyConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub encryption: String, // "starttls" | "tls" | "none"
    pub username: String,
    pub password: String,
    pub timeout: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiKeyConfig {
    pub api_key: String,
}

pub async fn send(cfg: &EmailConfig, msg: &AlertMessage) -> anyhow::Result<()> {
    if cfg.from_address.is_empty() || cfg.to_address.is_empty() {
        return Err(anyhow::anyhow!("from_address and to_address are required"));
    }
    let subject = format_subject(msg);
    let body_text = format_body_text(msg);
    let body_html = format_body_html(msg);

    match cfg.driver.as_str() {
        "smtp" => send_smtp(cfg, &subject, &body_text, &body_html).await,
        "brevo" => send_brevo(cfg, &subject, &body_html).await,
        "resend" => send_resend(cfg, &subject, &body_html).await,
        other => Err(anyhow::anyhow!("Unsupported email driver: {other}")),
    }
}

fn format_subject(msg: &AlertMessage) -> String {
    let sev = msg.severity.to_uppercase();
    let state = if msg.state == "resolved" {
        "RESOLVED"
    } else {
        sev.as_str()
    };
    format!("[Pier/{}] {}", state, msg.rule_name)
}

fn format_body_text(msg: &AlertMessage) -> String {
    let state_label = if msg.state == "resolved" {
        "RESOLVED".to_string()
    } else {
        format!("{} {}", severity_prefix(&msg.severity), msg.severity.to_uppercase())
    };
    let mut lines = vec![format!("{state_label} — {}", msg.rule_name)];
    if let Some(srv) = &msg.server_label {
        lines.push(format!("Server: {srv}"));
    }
    lines.push(format!("Scope: {}", msg.scope_label));

    let unit = match msg.metric.as_str() {
        "cpu" | "ram" | "disk" | "container_cpu" | "container_ram" => "%",
        "ssl_expiry" => " days",
        _ => "",
    };
    let label = format_metric_label(&msg.metric);
    if let (Some(val), Some(thr)) = (msg.value, msg.threshold) {
        lines.push(format!(
            "{label}: {val:.2}{unit} (threshold: {} {thr:.2}{unit})",
            msg.comparison
        ));
    } else if let Some(val) = msg.value {
        lines.push(format!("{label}: {val:.2}{unit}"));
    }
    if let Some(ctx) = &msg.context {
        if !ctx.is_empty() {
            lines.push(String::new());
            lines.push(ctx.clone());
        }
    }
    lines.push(format!("Time: {}", msg.time_str));
    lines.join("\n")
}

fn format_body_html(msg: &AlertMessage) -> String {
    let text = format_body_text(msg);
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "<br>");
    format!(
        "<html><body style=\"font-family: -apple-system, Segoe UI, Roboto, sans-serif; line-height: 1.5; color: #111;\"><div style=\"max-width: 560px; margin: 0 auto; padding: 20px;\">{escaped}</div></body></html>"
    )
}

async fn send_smtp(
    cfg: &EmailConfig,
    subject: &str,
    body_text: &str,
    body_html: &str,
) -> anyhow::Result<()> {
    if cfg.smtp.host.is_empty() {
        return Err(anyhow::anyhow!("SMTP host is required"));
    }
    let from: Mailbox = format!("{} <{}>", cfg.from_name, cfg.from_address)
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid from address: {e}"))?;
    let to: Mailbox = cfg
        .to_address
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid to address: {e}"))?;

    let email = Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .multipart(
            lettre::message::MultiPart::alternative()
                .singlepart(
                    lettre::message::SinglePart::builder()
                        .header(ContentType::TEXT_PLAIN)
                        .body(body_text.to_string()),
                )
                .singlepart(
                    lettre::message::SinglePart::builder()
                        .header(ContentType::TEXT_HTML)
                        .body(body_html.to_string()),
                ),
        )
        .map_err(|e| anyhow::anyhow!("Build email: {e}"))?;

    let port = if cfg.smtp.port == 0 { 587 } else { cfg.smtp.port };
    let timeout_secs = if cfg.smtp.timeout == 0 { 30 } else { cfg.smtp.timeout };

    let mut builder = match cfg.smtp.encryption.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp.host)
            .map_err(|e| anyhow::anyhow!("SMTP relay: {e}"))?,
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp.host),
        _ => {
            // default starttls
            let tls = TlsParameters::new(cfg.smtp.host.clone())
                .map_err(|e| anyhow::anyhow!("TLS params: {e}"))?;
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp.host)
                .map_err(|e| anyhow::anyhow!("SMTP starttls: {e}"))?
                .tls(Tls::Required(tls))
        }
    };

    builder = builder
        .port(port)
        .timeout(Some(Duration::from_secs(timeout_secs)));

    if !cfg.smtp.username.is_empty() {
        builder = builder.credentials(Credentials::new(
            cfg.smtp.username.clone(),
            cfg.smtp.password.clone(),
        ));
    }

    let mailer = builder.build();
    mailer
        .send(email)
        .await
        .map_err(|e| anyhow::anyhow!("SMTP send: {e}"))?;
    Ok(())
}

async fn send_brevo(cfg: &EmailConfig, subject: &str, body_html: &str) -> anyhow::Result<()> {
    if cfg.brevo.api_key.is_empty() {
        return Err(anyhow::anyhow!("Brevo api_key is required"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let body = serde_json::json!({
        "sender": { "name": cfg.from_name, "email": cfg.from_address },
        "to": [{ "email": cfg.to_address }],
        "subject": subject,
        "htmlContent": body_html,
    });
    let resp = client
        .post("https://api.brevo.com/v3/smtp/email")
        .header("api-key", &cfg.brevo.api_key)
        .header("accept", "application/json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Brevo API {status}: {body}"));
    }
    Ok(())
}

async fn send_resend(cfg: &EmailConfig, subject: &str, body_html: &str) -> anyhow::Result<()> {
    if cfg.resend.api_key.is_empty() {
        return Err(anyhow::anyhow!("Resend api_key is required"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let from = if cfg.from_name.is_empty() {
        cfg.from_address.clone()
    } else {
        format!("{} <{}>", cfg.from_name, cfg.from_address)
    };
    let body = serde_json::json!({
        "from": from,
        "to": [cfg.to_address],
        "subject": subject,
        "html": body_html,
    });
    let resp = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {}", cfg.resend.api_key))
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Resend API {status}: {body}"));
    }
    Ok(())
}
