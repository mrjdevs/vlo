use lettre::{
    message::{header::ContentType, Message},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
    SmtpTransport, Transport,
};
use serde::Serialize;
use std::env;

#[derive(Debug, Clone, PartialEq)]
pub enum MailDriver {
    Smtp,
    Api,
}

#[derive(Debug, Clone)]
pub struct MailConfig {
    pub enabled: bool,
    pub driver: MailDriver,
    pub from_email: String,
    pub from_name: String,
    pub reply_to: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub tls: String,
    pub api_url: String,
    pub api_token: String,
}

impl MailConfig {
    pub fn from_env() -> Self {
        let driver_str = env::var("MAIL_DRIVER").unwrap_or_else(|_| "smtp".to_string()).to_lowercase();
        let driver = if driver_str == "api" { MailDriver::Api } else { MailDriver::Smtp };

        let from_full = env::var("MAIL_FROM").unwrap_or_else(|_| "VLO <noreply@example.com>".to_string());
        let (from_name, from_email) = parse_email_address(&from_full);

        Self {
            enabled: env::var("MAIL_ENABLED").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            driver,
            from_email,
            from_name,
            reply_to: env::var("MAIL_REPLY_TO").ok().filter(|s| !s.is_empty()),
            host: env::var("MAIL_HOST").unwrap_or_else(|_| "smtp.example.com".to_string()),
            port: env::var("MAIL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(587),
            username: env::var("MAIL_USERNAME").unwrap_or_default(),
            password: env::var("MAIL_PASSWORD").unwrap_or_default(),
            tls: env::var("MAIL_TLS").unwrap_or_else(|_| "starttls".to_string()),
            api_url: env::var("MAIL_API_URL").unwrap_or_else(|_| "https://api.mailersend.com/v1/email".to_string()),
            api_token: env::var("MAIL_API_TOKEN").unwrap_or_default(),
        }
    }
}

fn parse_email_address(full: &str) -> (String, String) {
    if let Some(start) = full.find('<') {
        if let Some(end) = full.find('>') {
            let name = full[..start].trim().trim_matches('"').to_string();
            let email = full[start + 1..end].trim().to_string();
            return (name, email);
        }
    }
    ("VLO".to_string(), full.trim().to_string())
}

#[derive(Serialize)]
struct MailersendPayload {
    from: EmailAddr,
    to: Vec<EmailAddr>,
    subject: String,
    html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<EmailAddr>,
}

#[derive(Serialize)]
struct EmailAddr {
    email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

pub async fn send_email(to: &str, subject: &str, html_body: &str) -> Result<(), String> {
    let cfg = MailConfig::from_env();
    
    if !cfg.enabled {
        crate::vlo_debug!("📧 Mail disabled. Skipping email to {}: {}", to, subject);
        return Ok(());
    }

    match cfg.driver {
        MailDriver::Api => send_via_api(&cfg, to, subject, html_body).await,
        MailDriver::Smtp => {
            let to = to.to_string();
            let subject = subject.to_string();
            let html_body = html_body.to_string();
            // Offload blocking SMTP to a worker thread so we don't block the Tokio runtime
            tokio::task::spawn_blocking(move || send_via_smtp(&cfg, &to, &subject, &html_body))
                .await
                .map_err(|e| format!("Thread panicked: {}", e))?
        }
    }
}

async fn send_via_api(cfg: &MailConfig, to: &str, subject: &str, html_body: &str) -> Result<(), String> {
    let payload = MailersendPayload {
        from: EmailAddr {
            email: cfg.from_email.clone(),
            name: Some(cfg.from_name.clone()),
        },
        to: vec![EmailAddr {
            email: to.to_string(),
            name: None,
        }],
        subject: subject.to_string(),
        html: html_body.to_string(),
        reply_to: cfg.reply_to.as_ref().map(|r| EmailAddr { email: r.clone(), name: None }),
    };

    let client = reqwest::Client::new();
    let response = client
        .post(&cfg.api_url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", cfg.api_token))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("API request failed: {}", e))?;

    // MailerSend and most transactional APIs return 202 Accepted or 200 OK
    if response.status().is_success() || response.status().as_u16() == 202 {
        crate::vlo_debug!("✅ Email sent successfully via API to {}", to);
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("API returned status {}: {}", status, body))
    }
}

fn send_via_smtp(cfg: &MailConfig, to: &str, subject: &str, html_body: &str) -> Result<(), String> {
    let mut email_builder = Message::builder()
        .from(format!("{} <{}>", cfg.from_name, cfg.from_email).parse().map_err(|e| format!("Invalid MAIL_FROM: {}", e))?)
        .to(to.parse().map_err(|e| format!("Invalid recipient email: {}", e))?)
        .subject(subject)
        .header(ContentType::TEXT_HTML);

    if let Some(reply_to) = &cfg.reply_to {
        if let Ok(addr) = reply_to.parse() {
            email_builder = email_builder.reply_to(addr);
        }
    }

    let email = email_builder
        .body(html_body.to_string())
        .map_err(|e| format!("Failed to build email: {}", e))?;

    let tls = match cfg.tls.to_lowercase().as_str() {
        "tls" | "ssl" => Tls::Wrapper(TlsParameters::new(cfg.host.clone()).map_err(|e| e.to_string())?),
        "starttls" => Tls::Required(TlsParameters::new(cfg.host.clone()).map_err(|e| e.to_string())?),
        _ => Tls::None,
    };

    let mut mailer_builder = SmtpTransport::builder_dangerous(&cfg.host)
        .port(cfg.port)
        .tls(tls);

    if !cfg.username.is_empty() && !cfg.password.is_empty() {
        mailer_builder = mailer_builder.credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()));
    }

    let mailer = mailer_builder.build();
    mailer.send(&email).map_err(|e| format!("SMTP send failed: {}", e))?;
    
    crate::vlo_debug!("✅ Email sent successfully via SMTP to {}", to);
    Ok(())
}