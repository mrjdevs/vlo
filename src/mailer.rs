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

#[derive(Debug, Clone, PartialEq)]
pub enum ApiProvider {
    Resend,
    Mailjet,
    Sendgrid,
    Mailgun,
    Postmark,
    Brevo,
    Mailersend, // 🔥 Added explicit MailerSend support
    Custom,
}

#[derive(Debug, Clone)]
pub struct MailConfig {
    pub enabled: bool,
    pub driver: MailDriver,
    pub api_provider: ApiProvider,
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
    pub api_key: String,
    pub api_secret: String,
    pub api_domain: String,
}

impl MailConfig {
    pub fn from_env() -> Self {
        let driver_str = env::var("MAIL_DRIVER").unwrap_or_else(|_| "smtp".to_string()).to_lowercase();
        let driver = if driver_str == "api" { MailDriver::Api } else { MailDriver::Smtp };

        let provider_str = env::var("MAIL_API_PROVIDER").unwrap_or_else(|_| "custom".to_string()).to_lowercase();
        let api_provider = match provider_str.as_str() {
            "resend" => ApiProvider::Resend,
            "mailjet" => ApiProvider::Mailjet,
            "sendgrid" => ApiProvider::Sendgrid,
            "mailgun" => ApiProvider::Mailgun,
            "postmark" => ApiProvider::Postmark,
            "brevo" | "sendinblue" => ApiProvider::Brevo,
            "mailersend" => ApiProvider::Mailersend, // 🔥 Added
            _ => ApiProvider::Custom,
        };

        let from_full = env::var("MAIL_FROM").unwrap_or_else(|_| "VLO <noreply@example.com>".to_string());
        let (from_name, from_email) = parse_email_address(&from_full);

        Self {
            enabled: env::var("MAIL_ENABLED").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            driver,
            api_provider,
            from_email,
            from_name,
            reply_to: env::var("MAIL_REPLY_TO").ok().filter(|s| !s.is_empty()),
            host: env::var("MAIL_HOST").unwrap_or_else(|_| "smtp.example.com".to_string()),
            port: env::var("MAIL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(587),
            username: env::var("MAIL_USERNAME").unwrap_or_default(),
            password: env::var("MAIL_PASSWORD").unwrap_or_default(),
            tls: env::var("MAIL_TLS").unwrap_or_else(|_| "starttls".to_string()),
            api_url: env::var("MAIL_API_URL").unwrap_or_default(),
            api_token: env::var("MAIL_API_TOKEN").unwrap_or_default(),
            api_key: env::var("MAIL_API_KEY").unwrap_or_default(),
            api_secret: env::var("MAIL_API_SECRET").unwrap_or_default(),
            api_domain: env::var("MAIL_API_DOMAIN").unwrap_or_default(),
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

// ─── API Payload Structures ─────────────────────────────────────────────
#[derive(Serialize)]
struct ResendPayload {
    from: String,
    to: Vec<String>,
    subject: String,
    html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
}

#[derive(Serialize)]
struct MailjetPayload {
    messages: Vec<MailjetMessage>,
}

#[derive(Serialize)]
struct MailjetMessage {
    from: EmailAddr,
    to: Vec<EmailAddr>,
    subject: String,
    #[serde(rename = "HTMLPart")]
    html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<EmailAddr>,
}

#[derive(Serialize)]
struct SendgridPayload {
    personalizations: Vec<SendgridPersonalization>,
    from: EmailAddr,
    subject: String,
    content: Vec<SendgridContent>,
}

#[derive(Serialize)]
struct SendgridPersonalization {
    to: Vec<EmailAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<EmailAddr>,
}

#[derive(Serialize)]
struct SendgridContent {
    #[serde(rename = "type")]
    content_type: String,
    value: String,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct PostmarkPayload {
    From: String,
    To: String,
    Subject: String,
    HtmlBody: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ReplyTo: Option<String>,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct BrevoPayload {
    sender: BrevoContact,
    to: Vec<BrevoContact>,
    subject: String,
    htmlContent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    replyTo: Option<BrevoContact>,
}

#[derive(Serialize)]
struct BrevoContact {
    email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

// 🔥 MailerSend Payload Structure
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

pub struct EmailTemplate {
    pub subject: String,
    pub body: String,
}

pub fn render_email_template(template_name: &str, variables: &std::collections::HashMap<String, serde_json::Value>) -> Option<EmailTemplate> {
    let root = crate::state::get_project_root();
    let template_path = root.join("email_templates").join(format!("{}.html", template_name));
    
    if !template_path.exists() { 
        crate::vlo_debug!("📧 Email template '{}' not found in email_templates/. Using default.", template_name);
        return None; 
    }
    
    match std::fs::read_to_string(&template_path) {
        Ok(content) => {
            let subject_re = regex::Regex::new(r"(?i)<!--\s*subject:\s*(.*?)\s*-->").unwrap();
            let mut subject = String::new();
            let mut body = content.clone();
            if let Some(caps) = subject_re.captures(&content) {
                subject = caps.get(1).unwrap().as_str().to_string();
                body = subject_re.replace(&content, "").to_string();
            }
            let rendered_body = crate::template::render_interpolations(&body, variables);
            let rendered_body = crate::template::render_control_flow(&rendered_body, variables);
            let rendered_subject = if subject.trim().is_empty() { "Notification".to_string() } else {
                let s = crate::template::render_interpolations(&subject, variables);
                crate::template::render_control_flow(&s, variables)
            };
            Some(EmailTemplate { subject: rendered_subject, body: rendered_body })
        }
        Err(_) => None,
    }
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
            let to = to.to_string(); let subject = subject.to_string(); let html_body = html_body.to_string();
            tokio::task::spawn_blocking(move || send_via_smtp(&cfg, &to, &subject, &html_body))
                .await.map_err(|e| format!("Thread panicked: {}", e))?
        }
    }
}

async fn send_via_api(cfg: &MailConfig, to: &str, subject: &str, html_body: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let mut request = client.post(&cfg.api_url);

    match cfg.api_provider {
        ApiProvider::Resend => {
            let payload = ResendPayload {
                from: format!("{} <{}>", cfg.from_name, cfg.from_email), to: vec![to.to_string()],
                subject: subject.to_string(), html: html_body.to_string(), reply_to: cfg.reply_to.clone(),
            };
            request = request.bearer_auth(&cfg.api_token).header("Content-Type", "application/json").json(&payload);
        }
        ApiProvider::Mailjet => {
            let payload = MailjetPayload { messages: vec![MailjetMessage {
                from: EmailAddr { email: cfg.from_email.clone(), name: Some(cfg.from_name.clone()) },
                to: vec![EmailAddr { email: to.to_string(), name: None }], subject: subject.to_string(),
                html: html_body.to_string(), reply_to: cfg.reply_to.as_ref().map(|r| EmailAddr { email: r.clone(), name: None }),
            }]};
            request = request.basic_auth(&cfg.api_key, Some(&cfg.api_secret)).header("Content-Type", "application/json").json(&payload);
        }
        ApiProvider::Sendgrid => {
            let payload = SendgridPayload {
                personalizations: vec![SendgridPersonalization { to: vec![EmailAddr { email: to.to_string(), name: None }], reply_to: cfg.reply_to.as_ref().map(|r| EmailAddr { email: r.clone(), name: None }) }],
                from: EmailAddr { email: cfg.from_email.clone(), name: Some(cfg.from_name.clone()) }, subject: subject.to_string(),
                content: vec![SendgridContent { content_type: "text/html".to_string(), value: html_body.to_string() }],
            };
            request = request.bearer_auth(&cfg.api_token).header("Content-Type", "application/json").json(&payload);
        }
        ApiProvider::Mailgun => {
            if cfg.api_domain.is_empty() { return Err("MAIL_API_DOMAIN is required for Mailgun".to_string()); }
            let url = if cfg.api_url.is_empty() { format!("https://api.mailgun.net/v3/{}/messages", cfg.api_domain) } else { cfg.api_url.clone() };
            let mut form = std::collections::HashMap::new();
            form.insert("from", format!("{} <{}>", cfg.from_name, cfg.from_email));
            form.insert("to", to.to_string());
            form.insert("subject", subject.to_string());
            form.insert("html", html_body.to_string());
            if let Some(reply) = &cfg.reply_to { form.insert("h:Reply-To", reply.clone()); }
            request = client.post(&url).basic_auth("api", Some(&cfg.api_token)).form(&form);
        }
        ApiProvider::Postmark => {
            let payload = PostmarkPayload {
                From: format!("{} <{}>", cfg.from_name, cfg.from_email), To: to.to_string(),
                Subject: subject.to_string(), HtmlBody: html_body.to_string(), ReplyTo: cfg.reply_to.clone(),
            };
            request = request.header("X-Postmark-Server-Token", &cfg.api_token).header("Content-Type", "application/json").json(&payload);
        }
        ApiProvider::Brevo => {
            let payload = BrevoPayload {
                sender: BrevoContact { email: cfg.from_email.clone(), name: Some(cfg.from_name.clone()) },
                to: vec![BrevoContact { email: to.to_string(), name: None }], subject: subject.to_string(),
                htmlContent: html_body.to_string(),
                replyTo: cfg.reply_to.as_ref().map(|r| BrevoContact { email: r.clone(), name: None }),
            };
            request = request.header("api-key", &cfg.api_token).header("Content-Type", "application/json").json(&payload);
        }
        ApiProvider::Mailersend => {
            // 🔥 MailerSend requires the 'from' field to be an object, not a string
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
                reply_to: cfg.reply_to.as_ref().map(|r| EmailAddr {
                    email: r.clone(),
                    name: None,
                }),
            };
            request = request
                .bearer_auth(&cfg.api_token)
                .header("Content-Type", "application/json")
                .json(&payload);
        }
        ApiProvider::Custom => {
            request = request.bearer_auth(&cfg.api_token).header("Content-Type", "application/json").body(html_body.to_string());
        }
    }

    let response = request.send().await.map_err(|e| format!("API request failed: {}", e))?;
    if response.status().is_success() || response.status().as_u16() == 202 || response.status().as_u16() == 200 {
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
        .subject(subject).header(ContentType::TEXT_HTML);
    if let Some(reply_to) = &cfg.reply_to { if let Ok(addr) = reply_to.parse() { email_builder = email_builder.reply_to(addr); } }
    let email = email_builder.body(html_body.to_string()).map_err(|e| format!("Failed to build email: {}", e))?;

    let tls = match cfg.tls.to_lowercase().as_str() {
        "tls" | "ssl" => Tls::Wrapper(TlsParameters::new(cfg.host.clone()).map_err(|e| e.to_string())?),
        "starttls" => Tls::Required(TlsParameters::new(cfg.host.clone()).map_err(|e| e.to_string())?),
        _ => Tls::None,
    };

    let mut mailer_builder = SmtpTransport::builder_dangerous(&cfg.host).port(cfg.port).tls(tls);
    if !cfg.username.is_empty() && !cfg.password.is_empty() {
        mailer_builder = mailer_builder.credentials(Credentials::new(cfg.username.clone(), cfg.password.clone()));
    }
    mailer_builder.build().send(&email).map_err(|e| format!("SMTP send failed: {}", e))?;
    crate::vlo_debug!("✅ Email sent successfully via SMTP to {}", to);
    Ok(())
}