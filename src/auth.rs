use crate::component::parse_props_v7;
use crate::database::{DbPool, DB_POOL};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::{
    extract::{FromRequest, Request},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use hmac::{Hmac, Mac};
use regex::Regex;
use serde_json::json;
use sha2::Sha256;
use sqlx::FromRow;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user: Option<User>,
    pub csrf_token: Option<String>,
    pub session_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub user_table: String, pub user_id: String, pub user_identifier: String, pub user_password: String,
    pub user_name: String, pub user_email: String, pub user_role: String, pub user_status: String,
    pub session_table: String, pub session_token: String, pub session_user_id: String, pub session_expires_at: String,
    pub session_lifetime: i64, pub cookie_name: String, pub identifier_field: String, pub password_field: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            user_table: "users".into(), user_id: "id".into(), user_identifier: "email".into(), user_password: "password_hash".into(),
            user_name: String::new(), user_email: String::new(), user_role: String::new(), user_status: String::new(),
            session_table: "sessions".into(), session_token: "token".into(), session_user_id: "user_id".into(), session_expires_at: "expires_at".into(),
            session_lifetime: 86_400, cookie_name: "vlo_session".into(), identifier_field: "email".into(), password_field: "password".into(),
        }
    }
}

impl AuthConfig {
    pub fn from_env() -> Result<Self, String> {
        let d = Self::default();
        let cfg = Self {
            user_table: env_required("AUTH_USER_TABLE", d.user_table)?,
            user_id: env_required("AUTH_USER_ID", d.user_id)?,
            user_identifier: env_required("AUTH_USER_IDENTIFIER", d.user_identifier)?,
            user_password: env_required("AUTH_USER_PASSWORD", d.user_password)?,
            user_name: env_optional("AUTH_USER_NAME"), user_email: env_optional("AUTH_USER_EMAIL"),
            user_role: env_optional("AUTH_USER_ROLE"), user_status: env_optional("AUTH_USER_STATUS"),
            session_table: env("AUTH_SESSION_TABLE", d.session_table), session_token: env("AUTH_SESSION_TOKEN", d.session_token),
            session_user_id: env("AUTH_SESSION_USER_ID", d.session_user_id), session_expires_at: env("AUTH_SESSION_EXPIRES_AT", d.session_expires_at),
            session_lifetime: env_i64("AUTH_SESSION_LIFETIME", d.session_lifetime)?,
            cookie_name: env("AUTH_COOKIE", d.cookie_name), identifier_field: env("AUTH_IDENTIFIER_FIELD", d.identifier_field),
            password_field: env("AUTH_PASSWORD_FIELD", d.password_field),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in &[
            ("AUTH_USER_TABLE", &self.user_table), ("AUTH_USER_ID", &self.user_id),
            ("AUTH_USER_IDENTIFIER", &self.user_identifier), ("AUTH_USER_PASSWORD", &self.user_password),
            ("AUTH_SESSION_TABLE", &self.session_table), ("AUTH_SESSION_TOKEN", &self.session_token),
            ("AUTH_SESSION_USER_ID", &self.session_user_id), ("AUTH_SESSION_EXPIRES_AT", &self.session_expires_at),
        ] { validate_identifier(name, value)?; }
        
        if self.session_lifetime <= 0 { return Err("AUTH_SESSION_LIFETIME must be > 0".into()); }
        if self.cookie_name.trim().is_empty() { return Err("AUTH_COOKIE cannot be empty".into()); }
        Ok(())
    }

    pub fn placeholder(&self, pool: &DbPool, n: usize) -> String {
        match pool { DbPool::Postgres(_) => format!("${n}"), _ => "?".into() }
    }
}

fn env(name: &str, default: String) -> String { std::env::var(name).ok().filter(|v| !v.trim().is_empty()).map(|v| v.trim().to_owned()).unwrap_or(default) }
fn env_optional(name: &str) -> String { std::env::var(name).ok().map(|v| v.trim().to_owned()).unwrap_or_default() }
fn env_required(name: &str, default: String) -> Result<String, String> {
    let value = env(name, default);
    if value.trim().is_empty() { Err(format!("{name} cannot be empty")) } else { Ok(value) }
}
fn env_i64(name: &str, default: i64) -> Result<i64, String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().parse().map_err(|_| format!("{name} must be a valid integer")),
        _ => Ok(default),
    }
}
fn validate_identifier(name: &str, value: &str) -> Result<(), String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());
    if RE.is_match(value) { Ok(()) } else { Err(format!("Invalid {name} value '{value}'.")) }
}

static AUTH_CONFIG: OnceLock<AuthConfig> = OnceLock::new();
pub fn init_auth_config() {
    match AuthConfig::from_env() {
        Ok(cfg) => { AUTH_CONFIG.set(cfg).ok(); crate::vlo_debug!("✅ Auth config loaded"); }
        Err(e) => { eprintln!("❌ Auth config error: {e}"); std::process::exit(1); }
    }
}
pub fn auth_config() -> &'static AuthConfig { AUTH_CONFIG.get().expect("Auth config not initialized") }

static SESSION_SECRET: OnceLock<String> = OnceLock::new();
pub fn init_session_secret() {
    let secret = std::env::var("SESSION_SECRET").unwrap_or_else(|_| {
        let mut bytes = [0u8; 32]; OsRng.fill_bytes(&mut bytes);
        let secret = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        crate::vlo_debug!("⚠️ Generated random SESSION_SECRET");
        secret
    });
    SESSION_SECRET.set(secret).ok();
}
pub fn get_secret() -> &'static str { SESSION_SECRET.get().map(String::as_str).unwrap_or("default_secret") }

pub fn compute_csrf_token(session_token: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(get_secret().as_bytes()).expect("HMAC accepts any key size");
    mac.update(session_token.as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}
pub fn verify_csrf_token(session_token: &str, submitted: &str) -> bool {
    if submitted.is_empty() { return false; }
    let expected = compute_csrf_token(session_token);
    expected.len() == submitted.len() && expected.bytes().zip(submitted.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

#[allow(dead_code)]
pub async fn hash_password(password: &str) -> Result<String, String> {
    use argon2::{password_hash::{PasswordHasher, SaltString}, Argon2};
    Argon2::default().hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|h| h.to_string()).map_err(|e| format!("Failed to hash password: {e}"))
}
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::{password_hash::{PasswordHash, PasswordVerifier}, Argon2};
    PasswordHash::new(hash).map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok()).unwrap_or(false)
}

fn select_user_fields(cfg: &AuthConfig, alias: Option<&str>) -> String {
    let p = alias.map(|a| format!("{a}.")).unwrap_or_default();
    let name = if cfg.user_name.is_empty() { format!("{p}{} AS name", cfg.user_identifier) } else { format!("COALESCE({p}{}, {p}{}) AS name", cfg.user_name, cfg.user_identifier) };
    let email = if cfg.user_email.is_empty() { format!("{p}{} AS email", cfg.user_identifier) } else { format!("COALESCE({p}{}, {p}{}) AS email", cfg.user_email, cfg.user_identifier) };
    let role = if cfg.user_role.is_empty() { "'User' AS role".into() } else { format!("COALESCE({p}{}, 'User') AS role", cfg.user_role) };
    let status = if cfg.user_status.is_empty() { "'active' AS status".into() } else { format!("COALESCE({p}{}, 'active') AS status", cfg.user_status) };
    format!("{p}{} AS id, {name}, {email}, {role}, {status}", cfg.user_id)
}

fn status_condition(cfg: &AuthConfig, alias: &str) -> Option<String> {
    if cfg.user_status.is_empty() { None } else {
        Some(format!("LOWER(COALESCE({alias}.{}, 'active')) IN ('active','enabled','approved')", cfg.user_status))
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32]; OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn now_timestamp() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 }

pub async fn create_session(user_id: i64, remember_me: bool) -> Result<(String, i64), String> {
    let cfg = auth_config();
    let pool = DB_POOL.get().ok_or("Database not configured")?;
    let token = generate_token();
    let lifetime = if remember_me { 30 * 24 * 60 * 60 } else { cfg.session_lifetime };
    let expires_at = now_timestamp() + lifetime;
    let sql = format!("INSERT INTO {} ({}, {}, {}) VALUES ({}, {}, {})",
        cfg.session_table, cfg.session_token, cfg.session_user_id, cfg.session_expires_at,
        cfg.placeholder(pool, 1), cfg.placeholder(pool, 2), cfg.placeholder(pool, 3));
    
    let result = crate::db_execute!(pool, &sql, &token, user_id, expires_at).map(|_| ());
    result.map(|_| (token, lifetime)).map_err(|e| format!("Failed to create session: {e}"))
}

pub async fn get_user_from_session(token: &str) -> Option<User> {
    let cfg = auth_config();
    let pool = DB_POOL.get()?;
    let mut sql = format!("SELECT {} FROM {} u INNER JOIN {} s ON u.{} = s.{} WHERE s.{} = {} AND s.{} > {}",
        select_user_fields(cfg, Some("u")), cfg.user_table, cfg.session_table, cfg.user_id, cfg.session_user_id,
        cfg.session_token, cfg.placeholder(pool, 1), cfg.session_expires_at, cfg.placeholder(pool, 2));
    if let Some(condition) = status_condition(cfg, "u") { sql.push_str(" AND "); sql.push_str(&condition); }
    
    crate::db_fetch_optional_as!(pool, User, &sql, token, now_timestamp()).ok().flatten()
}

pub async fn delete_session(token: &str) -> Result<(), String> {
    let cfg = auth_config();
    let pool = DB_POOL.get().ok_or("Database not configured")?;
    let sql = format!("DELETE FROM {} WHERE {} = {}", cfg.session_table, cfg.session_token, cfg.placeholder(pool, 1));
    crate::db_execute!(pool, &sql, token).map(|_| ()).map_err(|e| format!("Failed to delete session: {e}"))
}

pub async fn cleanup_expired_sessions() -> Result<u64, String> {
    let cfg = auth_config();
    let pool = DB_POOL.get().ok_or("Database not configured")?;
    let sql = format!("DELETE FROM {} WHERE {} < {}", cfg.session_table, cfg.session_expires_at, cfg.placeholder(pool, 1));
    crate::db_execute!(pool, &sql, now_timestamp()).map_err(|e| format!("Failed to cleanup sessions: {e}"))
}

async fn find_user_by_identifier(identifier: &str) -> Option<User> {
    let cfg = auth_config();
    let pool = DB_POOL.get()?;
    let sql = format!("SELECT {} FROM {} u WHERE u.{} = {}",
        select_user_fields(cfg, Some("u")), cfg.user_table, cfg.user_identifier, cfg.placeholder(pool, 1));
    crate::db_fetch_optional_as!(pool, User, &sql, identifier).ok().flatten()
}

async fn fetch_password_hash(user_id: i64) -> Option<String> {
    let cfg = auth_config();
    let pool = DB_POOL.get()?;
    let sql = format!("SELECT {} FROM {} WHERE {} = {}", cfg.user_password, cfg.user_table, cfg.user_id, cfg.placeholder(pool, 1));
    crate::db_fetch_optional_scalar_string!(pool, &sql, user_id).ok().flatten()
}

pub fn parse_cookie_header(value: &str, cookie_name: &str) -> Option<String> {
    let prefix = format!("{cookie_name}=");
    value.split(';').map(str::trim).find_map(|p| p.strip_prefix(&prefix).map(str::to_owned))
}

pub async fn session_middleware(mut req: Request, next: Next) -> Response {
    let cfg = auth_config();
    let token = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|v| parse_cookie_header(v, &cfg.cookie_name));
    let (user, csrf_token) = match &token {
        Some(t) => (get_user_from_session(t).await, Some(compute_csrf_token(t))),
        None => (None, None),
    };
    req.extensions_mut().insert(AuthUser { user, csrf_token, session_token: token });
    next.run(req).await
}

pub async fn csrf_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    if matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) || path.starts_with("/api/auth/") || path.starts_with("/api/files/upload") {
        return next.run(req).await;
    }
    let token = match req.extensions().get::<AuthUser>() {
        Some(AuthUser { session_token: Some(token), user: Some(_), .. }) => token.clone(),
        _ => return next.run(req).await,
    };
    let submitted = req.headers().get("X-CSRF-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !verify_csrf_token(&token, submitted) {
        return (StatusCode::FORBIDDEN, Json(json!({"success": false, "error": "CSRF token invalid or missing"}))).into_response();
    }
    next.run(req).await
}

pub async fn api_auth_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    if !path.starts_with("/api/") || path.starts_with("/api/auth/") || path == "/api" || path.starts_with("/api/files/") {
        return next.run(req).await;
    }
    let public_routes = std::env::var("AUTH_API_PUBLIC_ROUTES").unwrap_or_default();
    let is_public = public_routes.split(',').map(str::trim).filter(|s| !s.is_empty()).any(|r| {
        if let Some(prefix) = r.strip_suffix('*') { path.starts_with(prefix) } 
        else { path == r || path.starts_with(&format!("{r}/")) }
    });
    if is_public { return next.run(req).await; }
    if req.extensions().get::<AuthUser>().and_then(|a| a.user.as_ref()).is_none() {
        return (StatusCode::UNAUTHORIZED, Json(json!({"success": false, "error": "Authentication required"}))).into_response();
    }
    next.run(req).await
}

fn login_error(status: StatusCode, error: &str, db_status: Option<&str>) -> Response {
    let mut body = json!({"success": false, "error": error});
    if let Some(db_status) = db_status {
        let activation_required = matches!(db_status.to_ascii_lowercase().as_str(), "pending" | "pending_activation" | "unverified" | "inactive" | "in-active");
        body["status"] = json!(db_status);
        body["activation_required"] = json!(activation_required);
    }
    (status, Json(body)).into_response()
}

pub async fn login_handler(req: Request) -> impl IntoResponse {
    let auth_cfg = auth_config(); // 🔥 FIX: Renamed from `cfg`
    let identifier_field = auth_cfg.identifier_field.clone();
    let password_field = auth_cfg.password_field.clone();
    let mut next_url = "/".to_owned();
    if let Some(q) = req.uri().query() {
        for pair in q.split('&') {
            let mut p = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (p.next(), p.next()) { if k == "next" { next_url = urlencoding::decode(v).unwrap_or_default().into_owned(); } }
        }
    }
    let client_ip = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("unknown").trim().to_owned())
        .or_else(|| req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string())).unwrap_or_else(|| "unknown".into());

    if !check_rate_limit(&client_ip) {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"success": false, "error": "Too many login attempts. Please try again in 60 seconds."}))).into_response();
    }

    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let mut identifier = String::new(); let mut password = String::new(); let mut remember_me = false;

    if content_type.contains("multipart/form-data") {
        let mut multipart = match axum::extract::Multipart::from_request(req, &()).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid multipart request"}))).into_response(),
        };
        while let Ok(Some(field)) = multipart.next_field().await {
            let name = field.name().unwrap_or("").to_owned(); let value = field.text().await.unwrap_or_default();
            match name.as_str() {
                x if x == identifier_field => identifier = value,
                x if x == password_field => password = value,
                "remember_me" => remember_me = matches!(value.to_lowercase().as_str(), "on" | "true" | "1"),
                "next" if !value.is_empty() => next_url = value, _ => {}
            }
        }
    } else if content_type.contains("application/json") {
        let payload = match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid JSON"}))).into_response(),
        };
        identifier = payload.get(&identifier_field).and_then(|v| v.as_str()).unwrap_or("").into();
        password = payload.get(&password_field).and_then(|v| v.as_str()).unwrap_or("").into();
        remember_me = payload.get("remember_me").and_then(|v| v.as_bool()).unwrap_or(false);
        if let Some(next) = payload.get("next").and_then(|v| v.as_str()) { if !next.is_empty() { next_url = next.into(); } }
    } else if content_type.contains("application/x-www-form-urlencoded") {
        let bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Failed to read body"}))).into_response(),
        };
        for pair in String::from_utf8_lossy(&bytes).split('&') {
            let mut p = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (p.next(), p.next()) {
                let value = urlencoding::decode(v).unwrap_or_default().replace('+', " ");
                match k {
                    x if x == identifier_field => identifier = value,
                    x if x == password_field => password = value,
                    "remember_me" => remember_me = matches!(value.to_lowercase().as_str(), "on" | "true" | "1"),
                    "next" if !value.is_empty() => next_url = value, _ => {}
                }
            }
        }
    } else {
        return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Unsupported content type"}))).into_response();
    }

    if identifier.is_empty() || password.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Credentials required"}))).into_response();
    }

    let user = match find_user_by_identifier(&identifier).await {
        Some(user) => user,
        None => return login_error(StatusCode::UNAUTHORIZED, "Invalid credentials", None),
    };

    let hash = match fetch_password_hash(user.id).await {
        Some(hash) => hash,
        None => return login_error(StatusCode::UNAUTHORIZED, "Invalid credentials", None),
    };

    if !verify_password(&password, &hash) {
        return login_error(StatusCode::UNAUTHORIZED, "Invalid credentials", None);
    }

    let active = matches!(user.status.to_ascii_lowercase().as_str(), "active" | "enabled" | "approved");
    if !active {
        return login_error(StatusCode::FORBIDDEN, "Account is not active", Some(&user.status));
    }

    let (token, lifetime) = match create_session(user.id, remember_me).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": e}))).into_response(),
    };

    if !next_url.starts_with('/') || next_url.starts_with("//") || next_url.contains("://") { next_url = "/".into(); }

    let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}", auth_cfg.cookie_name, token, lifetime);
    let mut response = Json(json!({
        "success": true, "user": { "id": user.id, "name": user.name, "email": user.email, "role": user.role, "status": user.status }, "redirect": next_url
    })).into_response();

    if let Ok(value) = HeaderValue::from_str(&cookie) { response.headers_mut().append(header::SET_COOKIE, value); }
    response
}

pub async fn logout_handler(req: Request) -> impl IntoResponse {
    let auth_cfg = auth_config(); // 🔥 FIX: Renamed from `cfg`
    let token = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|v| parse_cookie_header(v, &auth_cfg.cookie_name));
    if let Some(token) = token { let _ = delete_session(&token).await; }
    let cookie = format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", auth_cfg.cookie_name);
    let mut response = Redirect::to("/").into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) { response.headers_mut().append(header::SET_COOKIE, value); }
    response
}

pub async fn me_handler(axum::Extension(auth): axum::Extension<AuthUser>) -> impl IntoResponse {
    match auth.user {
        Some(user) => (StatusCode::OK, Json(json!({
            "authenticated": true, "user": { "id": user.id, "name": user.name, "email": user.email, "role": user.role, "status": user.status }
        }))).into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({"authenticated": false, "error": "Not authenticated"}))).into_response(),
    }
}

#[allow(dead_code)] pub fn is_authenticated(auth: &AuthUser) -> bool { auth.user.is_some() }
#[allow(dead_code)] pub fn has_role(auth: &AuthUser, role: &str) -> bool { auth.user.as_ref().is_some_and(|u| u.role.eq_ignore_ascii_case(role)) }
#[allow(dead_code)] pub fn require_auth(auth: &AuthUser) -> Result<(), Response> { if is_authenticated(auth) { Ok(()) } else { Err((StatusCode::UNAUTHORIZED, Json(json!({"success": false, "error": "Authentication required"}))).into_response()) } }
#[allow(dead_code)] pub fn require_role(auth: &AuthUser, role: &str) -> Result<(), Response> { if !is_authenticated(auth) { return Err((StatusCode::UNAUTHORIZED, Json(json!({"success": false, "error": "Authentication required"}))).into_response()); } if has_role(auth, role) { Ok(()) } else { Err((StatusCode::FORBIDDEN, Json(json!({"success": false, "error": "Insufficient permissions"}))).into_response()) } }

pub async fn register_handler(req: Request) -> impl IntoResponse {
    let auth_cfg = auth_config();
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let mut name = String::new(); let mut email = String::new(); let mut password = String::new();

    if content_type.contains("application/json") {
        let payload = match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid JSON"}))).into_response(),
        };
        name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().replace('+', " ");
        email = payload.get(&auth_cfg.identifier_field).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        password = payload.get(&auth_cfg.password_field).and_then(|v| v.as_str()).unwrap_or("").to_string();
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Failed to read body"}))).into_response(),
        };
        for pair in String::from_utf8_lossy(&bytes).split('&') {
            let mut p = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (p.next(), p.next()) {
                let value = urlencoding::decode(v).unwrap_or_default().replace('+', " ");
                match k { x if x == "name" => name = value, x if x == auth_cfg.identifier_field => email = value, x if x == auth_cfg.password_field => password = value, _ => {} }
            }
        }
    }

    if name.is_empty() || email.is_empty() || password.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Name, email, and password are required"}))).into_response();
    }

    let pool = match DB_POOL.get() { Some(p) => p, None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": "Database not configured"}))).into_response() };
    let hash = match hash_password(&password).await { Ok(h) => h, Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": e}))).into_response() };
    let status_col = if auth_cfg.user_status.is_empty() { "status" } else { &auth_cfg.user_status };
    let name_col = if auth_cfg.user_name.is_empty() { &auth_cfg.user_identifier } else { &auth_cfg.user_name };

    let sql = format!("INSERT INTO {} ({}, {}, {}, {}) VALUES ({}, {}, {}, 'pending')",
        auth_cfg.user_table, name_col, auth_cfg.user_identifier, auth_cfg.user_password, status_col,
        auth_cfg.placeholder(pool, 1), auth_cfg.placeholder(pool, 2), auth_cfg.placeholder(pool, 3));
    
    if let Err(_) = crate::db_execute!(pool, &sql, &name, &email, &hash).map(|_| ()) {  
        return (StatusCode::CONFLICT, Json(json!({
            "success": false, 
            "error": "Email already registered or DB error",
            "message": "Email already registered or DB error"
        }))).into_response();  
    }

    let fetch_sql = format!("SELECT {} FROM {} WHERE {} = {}", auth_cfg.user_id, auth_cfg.user_table, auth_cfg.user_identifier, auth_cfg.placeholder(pool, 1));
    // 🔥 FIX: Removed .flatten() because .ok() already returns Option<i64>
    let user_id: i64 = crate::db_fetch_one_scalar_i64!(pool, &fetch_sql, &email).ok().unwrap_or(0);

    let token = generate_token();
    let expires_at = now_timestamp() + (7 * 24 * 60 * 60);
    let token_sql = format!("INSERT INTO auth_tokens (user_id, token, type, expires_at) VALUES ({}, {}, 'activate', {})",
        auth_cfg.placeholder(pool, 1), auth_cfg.placeholder(pool, 2), auth_cfg.placeholder(pool, 3));
    let _ = crate::db_execute!(pool, &token_sql, user_id, &token, expires_at).map(|_| ());
    
    let root = crate::state::get_root_url();
    let activation_link = format!("{}/api/auth/activate?token={}", root, token);
    let app_name = std::env::var("APP_NAME").unwrap_or_else(|_| "VLO App".to_string());
    
    let mut vars = std::collections::HashMap::new();
    vars.insert("name".to_string(), serde_json::Value::String(name.clone()));
    vars.insert("email".to_string(), serde_json::Value::String(email.clone()));
    vars.insert("activation_link".to_string(), serde_json::Value::String(activation_link.clone()));
    vars.insert("app_name".to_string(), serde_json::Value::String(app_name.clone()));
    vars.insert("token".to_string(), serde_json::Value::String(token.clone()));
    
    let email_template = crate::mailer::render_email_template("welcome", &vars)
        .unwrap_or_else(|| crate::mailer::EmailTemplate {
            subject: format!("Activate your {} account", app_name),
            body: format!("<h2>Welcome to {}!</h2><p>Hi {},</p><p>Click <a href='{}'>here</a> to activate your account.</p>", app_name, name, activation_link),
        });
    
    // 🔥 CRITICAL: Do NOT put a semicolon after `true` or `false` here!
    let email_sent = match crate::mailer::send_email(&email, &email_template.subject, &email_template.body).await {
        Ok(_) => {
            crate::vlo_debug!("✅ Activation email queued for {}", email);
            true
        }
        Err(e) => {
            crate::vlo_debug!("❌ Failed to send activation email to {}: {}", email, e);
            false
        }
    };

    if email_sent {
        return (StatusCode::CREATED, Json(json!({
            "success": true, 
            "message": "Registration successful. Please check your email."
        }))).into_response();
    } else {
        return (StatusCode::CREATED, Json(json!({
            "success": true, 
            "warning": true,
            "message": "Account created, but we couldn't send the activation email. Please check your spam folder or try resending later."
        }))).into_response();
    }
}


pub async fn activate_handler(req: Request) -> impl IntoResponse {
    let query = req.uri().query().unwrap_or("");
    let token = query.split('&').find_map(|p| p.splitn(2, '=').nth(1)).unwrap_or("");
    let pool = match DB_POOL.get() { Some(p) => p, None => return (StatusCode::INTERNAL_SERVER_ERROR, "DB error").into_response() };
    let auth_cfg = auth_config(); // 🔥 FIX: Renamed from `cfg` to `auth_cfg`

    let p1 = auth_cfg.placeholder(pool, 1); let p2 = auth_cfg.placeholder(pool, 2);
    let fetch_sql = format!("SELECT user_id FROM auth_tokens WHERE token = {} AND type = 'activate' AND expires_at > {}", p1, p2);
    let user_id: Option<i64> = crate::db_fetch_optional_scalar_i64!(pool, &fetch_sql, token, now_timestamp()).ok().flatten();

    if let Some(uid) = user_id {
        let status_col = if auth_cfg.user_status.is_empty() { "status" } else { &auth_cfg.user_status };
        let update_sql = format!("UPDATE {} SET {} = 'active' WHERE {} = {}", auth_cfg.user_table, status_col, auth_cfg.user_id, auth_cfg.placeholder(pool, 1));
        let _ = crate::db_execute!(pool, &update_sql, uid).map(|_| ());
        
        let delete_sql = format!("DELETE FROM auth_tokens WHERE token = {}", auth_cfg.placeholder(pool, 1));
        let _ = crate::db_execute!(pool, &delete_sql, token).map(|_| ());
        
        let flash = crate::auth::encode_flash("success", "✅", "Account Activated", "Your account has been successfully activated. You can now log in.");
        let mut response = axum::response::Redirect::to("/login").into_response();
        if let Ok(val) = axum::http::HeaderValue::from_str(&crate::auth::flash_cookie_header(&flash)) { response.headers_mut().append(axum::http::header::SET_COOKIE, val); }
        return response;
    }
    let flash = crate::auth::encode_flash("error", "❌", "Activation Failed", "Invalid or expired activation token.");
    let mut response = axum::response::Redirect::to("/login").into_response();
    if let Ok(val) = axum::http::HeaderValue::from_str(&crate::auth::flash_cookie_header(&flash)) { response.headers_mut().append(axum::http::header::SET_COOKIE, val); }
    response
}

pub async fn forgot_password_handler(req: Request) -> impl IntoResponse {
    let cfg = auth_config();
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let client_ip = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("unknown").trim().to_owned())
        .or_else(|| req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_string())).unwrap_or_else(|| "unknown".into());

    if !check_rate_limit(&client_ip) { return (StatusCode::OK, Json(json!({"success": true, "message": "If an account with this email exists, a password reset link has been sent."}))).into_response(); }

    let mut identifier = String::new();
    if content_type.contains("application/json") {
        let payload = match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid JSON"}))).into_response(),
        };
        identifier = payload.get(&cfg.identifier_field).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Failed to read body"}))).into_response(),
        };
        for pair in String::from_utf8_lossy(&bytes).split('&') {
            let mut p = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (p.next(), p.next()) { if k == cfg.identifier_field { identifier = urlencoding::decode(v).unwrap_or_default().replace('+', " "); break; } }
        }
    }

    if identifier.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Email is required"}))).into_response(); }
    let pool = match DB_POOL.get() { Some(p) => p, None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": "Database not configured"}))).into_response() };

    let user = match find_user_by_identifier(&identifier).await {
        Some(u) => u,
        None => return (StatusCode::OK, Json(json!({"success": true, "message": "If an account with this email exists, a password reset link has been sent."}))).into_response(),
    };

    let token = generate_token();
    let expires_at = now_timestamp() + 3600;
    let token_sql = format!("INSERT INTO auth_tokens (user_id, token, type, expires_at) VALUES ({}, {}, 'password_reset', {})",
        cfg.placeholder(pool, 1), cfg.placeholder(pool, 2), cfg.placeholder(pool, 3));
    let _ = crate::db_execute!(pool, &token_sql, user.id, &token, expires_at).map(|_| ());

    let root = crate::state::get_root_url();
    let reset_link = format!("{}/login?token={}", root, token);
    let app_name = std::env::var("APP_NAME").unwrap_or_else(|_| "VLO App".to_string());
    
    let mut vars = std::collections::HashMap::new();
    vars.insert("email".to_string(), serde_json::Value::String(user.email.clone()));
    vars.insert("reset_link".to_string(), serde_json::Value::String(reset_link.clone()));
    vars.insert("app_name".to_string(), serde_json::Value::String(app_name.clone()));
    vars.insert("token".to_string(), serde_json::Value::String(token.clone()));
    
    let email_template = crate::mailer::render_email_template("reset_password", &vars)
        .unwrap_or_else(|| crate::mailer::EmailTemplate {
            subject: format!("Reset your {} password", app_name),
            body: format!("<h2>Password Reset Request</h2><p>Click <a href='{}'>here</a> to reset your password. This link expires in 1 hour.</p>", reset_link),
        });
    
    match crate::mailer::send_email(&user.email, &email_template.subject, &email_template.body).await {
        Ok(_) => crate::vlo_debug!("✅ Password reset email queued for {}", user.email),
        Err(e) => crate::vlo_debug!("❌ Failed to send reset email to {}: {}", user.email, e),
    }
    (StatusCode::OK, Json(json!({"success": true, "message": "If an account with this email exists, a password reset link has been sent."}))).into_response()
}

pub async fn reset_password_handler(req: Request) -> impl IntoResponse {
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let mut token = String::new(); let mut new_password = String::new();

    if content_type.contains("application/json") {
        let payload = match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid JSON"}))).into_response(),
        };
        token = payload.get("token").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        new_password = payload.get("new_password").and_then(|v| v.as_str()).unwrap_or("").to_string();
    } else if content_type.contains("application/x-www-form-urlencoded") {
        let bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Failed to read body"}))).into_response(),
        };
        for pair in String::from_utf8_lossy(&bytes).split('&') {
            let mut p = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (p.next(), p.next()) {
                let val = urlencoding::decode(v).unwrap_or_default().replace('+', " ");
                if k == "token" { token = val; } else if k == "new_password" { new_password = val; }
            }
        }
    } else { return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Unsupported content type"}))).into_response(); }

    if token.is_empty() || new_password.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Token and new password are required"}))).into_response(); }
    let pool = match DB_POOL.get() { Some(p) => p, None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": "Database not configured"}))).into_response() };

    let auth_cfg = auth_config(); // 🔥 FIX: Renamed from `cfg` to `auth_cfg`
    let p1 = auth_cfg.placeholder(pool, 1); let p2 = auth_cfg.placeholder(pool, 2);
    let fetch_sql = format!("SELECT user_id FROM auth_tokens WHERE token = {} AND type = 'password_reset' AND expires_at > {}", p1, p2);
    let user_id: Option<i64> = crate::db_fetch_optional_scalar_i64!(pool, &fetch_sql, &token, now_timestamp()).ok().flatten();

    let user_id = match user_id { Some(id) => id, None => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid or expired reset token"}))).into_response() };
    let hash = match hash_password(&new_password).await { Ok(h) => h, Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": e}))).into_response() };

    let update_sql = format!("UPDATE {} SET {} = {} WHERE {} = {}", auth_cfg.user_table, auth_cfg.user_password, auth_cfg.placeholder(pool, 1), auth_cfg.user_id, auth_cfg.placeholder(pool, 2));
    if let Err(e) = crate::db_execute!(pool, &update_sql, &hash, user_id).map(|_| ()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": format!("Failed to update password: {}", e)}))).into_response();
    }

    let delete_token_sql = format!("DELETE FROM auth_tokens WHERE token = {}", auth_cfg.placeholder(pool, 1));
    let _ = crate::db_execute!(pool, &delete_token_sql, &token).map(|_| ());

    let delete_sessions_sql = format!("DELETE FROM {} WHERE {} = {}", auth_cfg.session_table, auth_cfg.session_user_id, auth_cfg.placeholder(pool, 1));
    let _ = crate::db_execute!(pool, &delete_sessions_sql, user_id).map(|_| ());

    (StatusCode::OK, Json(json!({"success": true, "message": "Password reset successfully. You can now log in."}))).into_response()
}

pub async fn resend_activation_handler(req: Request) -> impl IntoResponse {
    let cfg = auth_config();
    let content_type = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    let mut identifier = String::new();
    if content_type.contains("application/json") {
        let payload = match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Invalid JSON"}))).into_response(),
        };
        identifier = payload.get(&cfg.identifier_field).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(v) => v, Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Failed to read body"}))).into_response(),
        };
        for pair in String::from_utf8_lossy(&bytes).split('&') {
            let mut p = pair.splitn(2, '=');
            if let (Some(k), Some(v)) = (p.next(), p.next()) { if k == cfg.identifier_field { identifier = urlencoding::decode(v).unwrap_or_default().replace('+', " "); break; } }
        }
    }

    if identifier.is_empty() { return (StatusCode::BAD_REQUEST, Json(json!({"success": false, "error": "Email is required"}))).into_response(); }
    let pool = match DB_POOL.get() { Some(p) => p, None => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success": false, "error": "Database not configured"}))).into_response() };

    let user = match find_user_by_identifier(&identifier).await {
        Some(u) => u,
        None => return (StatusCode::OK, Json(json!({"success": true, "message": "If an account exists, an activation link has been sent."}))).into_response(),
    };

    if user.status.to_lowercase() == "active" { return (StatusCode::OK, Json(json!({"success": true, "message": "Your account is already active. You can log in."}))).into_response(); }

    let token = generate_token();
    let expires_at = now_timestamp() + (7 * 24 * 60 * 60);
    let token_sql = format!("INSERT INTO auth_tokens (user_id, token, type, expires_at) VALUES ({}, {}, 'activate', {})",
        cfg.placeholder(pool, 1), cfg.placeholder(pool, 2), cfg.placeholder(pool, 3));
    let _ = crate::db_execute!(pool, &token_sql, user.id, &token, expires_at).map(|_| ());

    let root = crate::state::get_root_url();
    let activation_link = format!("{}/api/auth/activate?token={}", root, token);
    let app_name = std::env::var("APP_NAME").unwrap_or_else(|_| "VLO App".to_string());
    
    let mut vars = std::collections::HashMap::new();
    vars.insert("email".to_string(), serde_json::Value::String(user.email.clone()));
    vars.insert("activation_link".to_string(), serde_json::Value::String(activation_link.clone()));
    vars.insert("app_name".to_string(), serde_json::Value::String(app_name.clone()));
    vars.insert("token".to_string(), serde_json::Value::String(token.clone()));
    
    let email_template = crate::mailer::render_email_template("activation_reminder", &vars)
        .unwrap_or_else(|| crate::mailer::EmailTemplate {
            subject: format!("Activate your {} account", app_name),
            body: format!("<h2>Activate Your Account</h2><p>Click <a href='{}'>here</a> to activate your account.</p>", activation_link),
        });
    
    match crate::mailer::send_email(&user.email, &email_template.subject, &email_template.body).await {
        Ok(_) => crate::vlo_debug!("✅ Activation email queued for {}", user.email),
        Err(e) => crate::vlo_debug!("❌ Failed to send activation email to {}: {}", user.email, e),
    }
    (StatusCode::OK, Json(json!({"success": true, "message": "Activation link sent successfully. Please check your email."}))).into_response()
}

static LAYOUT_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)<([A-Za-z][A-Za-z0-9_-]*)Layout\s+([^>]*)>"#).unwrap());
pub struct PageGuard { pub auth: bool, pub roles: Vec<String>, pub guest: bool }
pub fn extract_page_guard(source: &str) -> Option<PageGuard> {
    let caps = LAYOUT_TAG_RE.captures(source)?;
    let props = parse_props_v7(caps.get(2)?.as_str());
    let bool_prop = |key: &str| match props.get(key) { Some(serde_json::Value::Bool(v)) => *v, Some(serde_json::Value::String(v)) => v.eq_ignore_ascii_case("true"), _ => false };
    let auth = bool_prop("auth"); let guest = bool_prop("guest");
    let mut roles = Vec::new();
    for key in ["roles", "role"] { if let Some(serde_json::Value::String(value)) = props.get(key) { roles.extend(value.split(',').map(str::trim).filter(|v| !v.is_empty()).map(String::from)); } }
    (auth || guest || !roles.is_empty()).then_some(PageGuard { auth, roles, guest })
}
fn safe_next(next: Option<&str>) -> String { match next { Some(v) if v.starts_with('/') && !v.starts_with("/login") && !v.contains("://") => v.into(), _ => "/dashboard".into() } }
pub fn check_page_guard(guard: &PageGuard, user: &Option<User>, current_path: &str, next: Option<&str>) -> Option<Response> {
    if guard.guest && user.is_some() { return Some(Redirect::to(&safe_next(next)).into_response()); }
    if guard.auth && user.is_none() { return Some(Redirect::to(&format!("/login?next={current_path}")).into_response()); }
    if !guard.roles.is_empty() {
        let allowed = user.as_ref().is_some_and(|u| guard.roles.iter().any(|r| r.eq_ignore_ascii_case(&u.role)));
        if !allowed {
            if user.is_none() { return Some(Redirect::to(&format!("/login?next={current_path}")).into_response()); }
            return Some((StatusCode::FORBIDDEN, Html("<h1>403 Forbidden</h1><p>Insufficient permissions.</p>".to_string())).into_response());
        }
    }
    None
}

pub const FLASH_COOKIE: &str = "vlo_flash";
pub fn encode_flash(variant: &str, icon: &str, title: &str, description: &str) -> String { urlencoding::encode(&serde_json::to_string(&json!({"variant": variant, "icon": icon, "title": title, "description": description})).unwrap_or_default()).to_string() }
pub fn decode_flash(encoded: &str) -> Option<serde_json::Value> { let decoded = urlencoding::decode(encoded).ok()?; serde_json::from_str(&decoded).ok() }
pub fn flash_cookie_header(encoded: &str) -> String { format!("{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=60", FLASH_COOKIE, encoded) }
pub fn expire_flash_cookie() -> String { format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", FLASH_COOKIE) }

static RATE_LIMITS: LazyLock<Mutex<HashMap<String, Vec<SystemTime>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_MAX_ATTEMPTS: usize = 5;
pub fn check_rate_limit(ip: &str) -> bool {
    let now = SystemTime::now(); let mut limits = RATE_LIMITS.lock().unwrap(); let entries = limits.entry(ip.into()).or_default();
    entries.retain(|time| now.duration_since(*time).unwrap_or_default() < Duration::from_secs(RATE_LIMIT_WINDOW_SECS));
    if entries.len() >= RATE_LIMIT_MAX_ATTEMPTS { return false; } entries.push(now); true
}
#[allow(dead_code)] pub fn cleanup_rate_limits() {
    let now = SystemTime::now(); let mut limits = RATE_LIMITS.lock().unwrap();
    limits.retain(|_, entries| { entries.retain(|time| now.duration_since(*time).unwrap_or_default() < Duration::from_secs(RATE_LIMIT_WINDOW_SECS)); !entries.is_empty() });
}