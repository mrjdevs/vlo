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
    sync::{LazyLock, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user: Option<User>,
    pub csrf_token: Option<String>,
    pub session_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Auth Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub user_table: String,
    pub user_id: String,
    pub user_name: String,
    pub user_identifier: String,
    pub user_password: String,
    pub user_role: String,
    pub session_table: String,
    pub session_token: String,
    pub session_user_id: String,
    pub session_expires_at: String,
    pub session_lifetime: i64,
    pub cookie_name: String,
    pub identifier_field: String,
    pub password_field: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            user_table: "users".into(),
            user_id: "id".into(),
            user_name: "name".into(),
            user_identifier: "email".into(),
            user_password: "password_hash".into(),
            user_role: "role".into(),
            session_table: "sessions".into(),
            session_token: "token".into(),
            session_user_id: "user_id".into(),
            session_expires_at: "expires_at".into(),
            session_lifetime: 86400,
            cookie_name: "vlo_session".into(),
            identifier_field: "email".into(),
            password_field: "password".into(),
        }
    }
}

impl AuthConfig {
    pub fn from_env() -> Result<Self, String> {
        let d = Self::default();
        let c = Self {
            user_table: env("AUTH_USER_TABLE", d.user_table),
            user_id: env("AUTH_USER_ID", d.user_id),
            user_name: env("AUTH_USER_NAME", d.user_name),
            user_identifier: env("AUTH_USER_IDENTIFIER", d.user_identifier),
            user_password: env("AUTH_USER_PASSWORD", d.user_password),
            user_role: env("AUTH_USER_ROLE", d.user_role),
            session_table: env("AUTH_SESSION_TABLE", d.session_table),
            session_token: env("AUTH_SESSION_TOKEN", d.session_token),
            session_user_id: env("AUTH_SESSION_USER_ID", d.session_user_id),
            session_expires_at: env("AUTH_SESSION_EXPIRES_AT", d.session_expires_at),
            session_lifetime: env_i64("AUTH_SESSION_LIFETIME", d.session_lifetime)?,
            cookie_name: env("AUTH_COOKIE", d.cookie_name),
            identifier_field: env("AUTH_IDENTIFIER_FIELD", d.identifier_field),
            password_field: env("AUTH_PASSWORD_FIELD", d.password_field),
        };
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("AUTH_USER_TABLE", &self.user_table),
            ("AUTH_USER_ID", &self.user_id),
            ("AUTH_USER_NAME", &self.user_name),
            ("AUTH_USER_IDENTIFIER", &self.user_identifier),
            ("AUTH_USER_PASSWORD", &self.user_password),
            ("AUTH_USER_ROLE", &self.user_role),
            ("AUTH_SESSION_TABLE", &self.session_table),
            ("AUTH_SESSION_TOKEN", &self.session_token),
            ("AUTH_SESSION_USER_ID", &self.session_user_id),
            ("AUTH_SESSION_EXPIRES_AT", &self.session_expires_at),
        ] {
            if !value.is_empty() {
                validate_identifier(name, value)?;
            }
        }
        if self.session_lifetime <= 0 {
            return Err("AUTH_SESSION_LIFETIME must be greater than zero".into());
        }
        if self.cookie_name.trim().is_empty() {
            return Err("AUTH_COOKIE cannot be empty".into());
        }
        Ok(())
    }

    fn placeholder(&self, pool: &DbPool, index: usize) -> String {
        match pool {
            DbPool::Postgres(_) => format!("${}", index),
            _ => "?".to_string(),
        }
    }
}

fn env(name: &str, default: String) -> String {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v.trim().into(),
        _ => default,
    }
}

fn env_i64(name: &str, default: i64) -> Result<i64, String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse()
            .map_err(|_| format!("{} must be a valid integer", name)),
        _ => Ok(default),
    }
}

fn validate_identifier(name: &str, value: &str) -> Result<(), String> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());
    if RE.is_match(value) {
        Ok(())
    } else {
        Err(format!(
            "Invalid {} value '{}'. SQL identifiers may contain only letters, numbers and underscores and must not start with a number.",
            name, value
        ))
    }
}

static AUTH_CONFIG: OnceLock<AuthConfig> = OnceLock::new();

pub fn init_auth_config() {
    match AuthConfig::from_env() {
        Ok(cfg) => {
            AUTH_CONFIG.set(cfg).ok();
            crate::vlo_debug!("✅ Auth config loaded from ENV");
        }
        Err(e) => {
            eprintln!("❌ Auth config error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn auth_config() -> &'static AuthConfig {
    AUTH_CONFIG.get().expect("Auth config not initialized")
}

// ---------------------------------------------------------------------------
// Session Secret
// ---------------------------------------------------------------------------

static SESSION_SECRET: OnceLock<String> = OnceLock::new();

pub fn init_session_secret() {
    let secret = std::env::var("SESSION_SECRET").unwrap_or_else(|_| {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let s: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        crate::vlo_debug!("⚠️ VLO DEBUG: Generated random SESSION_SECRET (set in .env for persistence)");
        s
    });
    SESSION_SECRET.set(secret).ok();
}

pub fn get_secret() -> &'static str {
    SESSION_SECRET.get().map(|s| s.as_str()).unwrap_or("default_secret")
}

// ---------------------------------------------------------------------------
// CSRF Tokens
// ---------------------------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

pub fn compute_csrf_token(session_token: &str) -> String {
    let secret = get_secret();
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key size");
    mac.update(session_token.as_bytes());
    let result = mac.finalize().into_bytes();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn verify_csrf_token(session_token: &str, submitted: &str) -> bool {
    if submitted.is_empty() {
        return false;
    }
    let expected = compute_csrf_token(session_token);
    if expected.len() != submitted.len() {
        return false;
    }
    expected
        .bytes()
        .zip(submitted.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// ---------------------------------------------------------------------------
// Password Hashing
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub async fn hash_password(password: &str) -> Result<String, String> {
    use argon2::{
        password_hash::{PasswordHasher, SaltString},
        Argon2,
    };
    Argon2::default()
        .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
        .map(|h| h.to_string())
        .map_err(|e| format!("Failed to hash password: {}", e))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::{
        password_hash::{PasswordHash, PasswordVerifier},
        Argon2,
    };
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Session Management
// ---------------------------------------------------------------------------

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn now_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub async fn create_session(user_id: i64) -> Result<String, String> {
    let cfg = auth_config();
    let pool = DB_POOL.get().ok_or("Database not configured")?;
    let token = generate_token();
    let expires_at = now_timestamp() + cfg.session_lifetime;
    let p1 = cfg.placeholder(pool, 1);
    let p2 = cfg.placeholder(pool, 2);
    let p3 = cfg.placeholder(pool, 3);
    let sql = format!(
        "INSERT INTO {} ({}, {}, {}) VALUES ({}, {}, {})",
        cfg.session_table, cfg.session_token, cfg.session_user_id, cfg.session_expires_at, p1, p2, p3
    );
    let result = match pool {
        DbPool::Sqlite(c) => sqlx::query(&sql).bind(&token).bind(user_id).bind(expires_at).execute(c).await.map(|_| ()),
        DbPool::Postgres(c) => sqlx::query(&sql).bind(&token).bind(user_id).bind(expires_at).execute(c).await.map(|_| ()),
        DbPool::MySql(c) => sqlx::query(&sql).bind(&token).bind(user_id).bind(expires_at).execute(c).await.map(|_| ()),
    };
    result.map_err(|e| format!("Failed to create session: {}", e))?;
    Ok(token)
}

pub async fn get_user_from_session(token: &str) -> Option<User> {
    let cfg = auth_config();
    let pool = DB_POOL.get()?;
    let now = now_timestamp();
    let p1 = cfg.placeholder(pool, 1);
    let p2 = cfg.placeholder(pool, 2);
    let role_expr = if cfg.user_role.is_empty() {
        "'User' AS role".to_string()
    } else {
        format!("COALESCE(u.{}, 'User') AS role", cfg.user_role)
    };
    let sql = format!(
        "SELECT u.{id} AS id, u.{name} AS name, u.{ident} AS email, {role} \
         FROM {ut} u INNER JOIN {st} s ON u.{id} = s.{suc} \
         WHERE s.{stc} = {p1} AND s.{sec} > {p2}",
        id = cfg.user_id,
        name = cfg.user_name,
        ident = cfg.user_identifier,
        role = role_expr,
        ut = cfg.user_table,
        st = cfg.session_table,
        suc = cfg.session_user_id,
        stc = cfg.session_token,
        sec = cfg.session_expires_at,
        p1 = p1,
        p2 = p2
    );

    crate::vlo_debug!("🔐 DB: Query: {}", sql);
    crate::vlo_debug!("🔐 DB: Token (first 8): {}", &token[..8.min(token.len())]);
    crate::vlo_debug!("🔐 DB: Now timestamp: {}", now);

    let result = match pool {
        DbPool::Sqlite(c) => sqlx::query_as::<_, User>(&sql).bind(token).bind(now).fetch_optional(c).await,
        DbPool::Postgres(c) => sqlx::query_as::<_, User>(&sql).bind(token).bind(now).fetch_optional(c).await,
        DbPool::MySql(c) => sqlx::query_as::<_, User>(&sql).bind(token).bind(now).fetch_optional(c).await,
    };

    match &result {
        Ok(Some(user)) => crate::vlo_debug!("🔐 DB: Found user: {} ({})", user.name, user.id),
        Ok(None) => crate::vlo_debug!("🔐 DB: No user found for this token"),
        Err(e) => crate::vlo_debug!("🔐 DB: Query error: {}", e),
    }

    result.ok().flatten()
}

pub async fn delete_session(token: &str) -> Result<(), String> {
    let cfg = auth_config();
    let pool = DB_POOL.get().ok_or("Database not configured")?;
    let p1 = cfg.placeholder(pool, 1);
    let sql = format!("DELETE FROM {} WHERE {} = {}", cfg.session_table, cfg.session_token, p1);
    let result = match pool {
        DbPool::Sqlite(c) => sqlx::query(&sql).bind(token).execute(c).await.map(|_| ()),
        DbPool::Postgres(c) => sqlx::query(&sql).bind(token).execute(c).await.map(|_| ()),
        DbPool::MySql(c) => sqlx::query(&sql).bind(token).execute(c).await.map(|_| ()),
    };
    result.map_err(|e| format!("Failed to delete session: {}", e))?;
    Ok(())
}

async fn find_user_by_identifier(identifier: &str) -> Option<User> {
    let cfg = auth_config();
    let pool = DB_POOL.get()?;
    let p1 = cfg.placeholder(pool, 1);
    let role_expr = if cfg.user_role.is_empty() {
        "'User' AS role".to_string()
    } else {
        format!("COALESCE({}, 'User') AS role", cfg.user_role)
    };
    let sql = format!(
        "SELECT {id} AS id, {name} AS name, {ident} AS email, {role} FROM {ut} WHERE {ident} = {p1}",
        id = cfg.user_id,
        name = cfg.user_name,
        ident = cfg.user_identifier,
        role = role_expr,
        ut = cfg.user_table,
        p1 = p1
    );
    match pool {
        DbPool::Sqlite(c) => sqlx::query_as::<_, User>(&sql).bind(identifier).fetch_optional(c).await,
        DbPool::Postgres(c) => sqlx::query_as::<_, User>(&sql).bind(identifier).fetch_optional(c).await,
        DbPool::MySql(c) => sqlx::query_as::<_, User>(&sql).bind(identifier).fetch_optional(c).await,
    }
    .ok()
    .flatten()
}

async fn fetch_password_hash(user_id: i64) -> Option<String> {
    let cfg = auth_config();
    let pool = DB_POOL.get()?;
    let p1 = cfg.placeholder(pool, 1);
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = {}",
        cfg.user_password, cfg.user_table, cfg.user_id, p1
    );
    match pool {
        DbPool::Sqlite(c) => sqlx::query_scalar::<_, String>(&sql).bind(user_id).fetch_optional(c).await,
        DbPool::Postgres(c) => sqlx::query_scalar::<_, String>(&sql).bind(user_id).fetch_optional(c).await,
        DbPool::MySql(c) => sqlx::query_scalar::<_, String>(&sql).bind(user_id).fetch_optional(c).await,
    }
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

fn parse_cookie_header(value: &str, cookie_name: &str) -> Option<String> {
    let prefix = format!("{}=", cookie_name);
    crate::vlo_debug!("🔐 COOKIE: Looking for prefix '{}' in '{}'", prefix, value);

    let result = value
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&prefix).map(str::to_string));

    crate::vlo_debug!(
        "🔐 COOKIE: Parse result: {:?}",
        result.as_ref().map(|t| &t[..8.min(t.len())])
    );
    result
}

pub async fn session_middleware(mut req: Request, next: Next) -> Response {
    let cookie_name = auth_config().cookie_name.clone();

    let cookie_header = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok());

    crate::vlo_debug!("🔐 SESSION: Cookie header: {:?}", cookie_header);

    let session_token = cookie_header.and_then(|h| parse_cookie_header(h, &cookie_name));

    crate::vlo_debug!(
        "🔐 SESSION: Extracted token: {:?}",
        session_token.as_ref().map(|t| &t[..8.min(t.len())])
    );

    let (user, csrf_token) = match &session_token {
        Some(t) => {
            let found_user = get_user_from_session(t).await;
            crate::vlo_debug!("🔐 SESSION: User lookup result: {}", found_user.is_some());
            (found_user, Some(compute_csrf_token(t)))
        }
        None => {
            crate::vlo_debug!("🔐 SESSION: No session token found");
            (None, None)
        }
    };

    req.extensions_mut().insert(AuthUser {
        user,
        csrf_token,
        session_token,
    });
    next.run(req).await
}

// ---------------------------------------------------------------------------
// CSRF Middleware
// ---------------------------------------------------------------------------

pub async fn csrf_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    let is_safe = matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    let is_exempt = path.starts_with("/api/auth/") || path.starts_with("/api/files/upload");

    if is_safe || is_exempt {
        return next.run(req).await;
    }

    let auth = req.extensions().get::<AuthUser>().cloned();
    let session_token = match auth {
        Some(AuthUser { session_token: Some(t), user: Some(_), .. }) => t,
        _ => return next.run(req).await,
    };

    let submitted = req
        .headers()
        .get("X-CSRF-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !verify_csrf_token(&session_token, submitted) {
        crate::vlo_debug!("🛡️ CSRF rejected for {} {}", method, path);
        return (StatusCode::FORBIDDEN, Json(json!({
            "success": false,
            "error": "CSRF token invalid or missing"
        })))
            .into_response();
    }

    next.run(req).await
}

// ---------------------------------------------------------------------------
// API Auth Middleware
// ---------------------------------------------------------------------------

pub async fn api_auth_middleware(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    // Only protect API routes
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }

    // Exempt routes
    let is_exempt = path.starts_with("/api/auth/")
        || path == "/api"
        || path.starts_with("/api/files/");

    if is_exempt {
        return next.run(req).await;
    }

    // Check public routes from ENV
    let public_env = std::env::var("AUTH_API_PUBLIC_ROUTES").unwrap_or_default();
    let public_routes: Vec<&str> = public_env
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let is_public = public_routes.iter().any(|r| path.starts_with(r));

    if is_public {
        return next.run(req).await;
    }

    // Check if user is authenticated
    let auth = req.extensions().get::<AuthUser>().cloned();
    let is_authenticated = auth.as_ref().map_or(false, |a| a.user.is_some());

    if !is_authenticated {
        crate::vlo_debug!("🔒 API auth rejected: {} {}", method, path);
        return (StatusCode::UNAUTHORIZED, Json(json!({
            "success": false,
            "error": "Authentication required"
        })))
            .into_response();
    }

    next.run(req).await
}

// ---------------------------------------------------------------------------
// Login / Logout / Me
// ---------------------------------------------------------------------------

pub async fn login_handler(req: Request) -> impl IntoResponse {
    let cfg = auth_config();
    let id_field = cfg.identifier_field.clone();
    let pw_field = cfg.password_field.clone();

    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let mut identifier = String::new();
    let mut password = String::new();

    if content_type.contains("multipart/form-data") {
        match axum::extract::Multipart::from_request(req, &()).await {
            Ok(mut multipart) => {
                while let Ok(Some(field)) = multipart.next_field().await {
                    let name = field.name().unwrap_or("").to_string();
                    let value = field.text().await.unwrap_or_default();
                    if name == id_field { identifier = value; }
                    else if name == pw_field { password = value; }
                }
            }
            Err(_) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"success":false,"error":"Invalid multipart request"})))
                    .into_response()
            }
        }
    } else if content_type.contains("application/json") {
        match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(payload) => {
                identifier = payload.get(&id_field).and_then(|v| v.as_str()).unwrap_or("").into();
                password = payload.get(&pw_field).and_then(|v| v.as_str()).unwrap_or("").into();
            }
            Err(_) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"success":false,"error":"Invalid JSON"})))
                    .into_response()
            }
        }
    } else if content_type.contains("application/x-www-form-urlencoded") {
        match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(bytes) => {
                for pair in String::from_utf8_lossy(&bytes).split('&') {
                    let mut p = pair.splitn(2, '=');
                    if let (Some(k), Some(v)) = (p.next(), p.next()) {
                        let v = urlencoding::decode(v).unwrap_or_default().to_string();
                        if k == id_field { identifier = v; }
                        else if k == pw_field { password = v; }
                    }
                }
            }
            Err(_) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"success":false,"error":"Failed to read body"})))
                    .into_response()
            }
        }
    } else {
        return (StatusCode::BAD_REQUEST, Json(json!({"success":false,"error":"Unsupported content type"})))
            .into_response();
    }

    if identifier.is_empty() || password.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"success":false,"error":"Credentials required"})))
            .into_response();
    }

    let user = match find_user_by_identifier(&identifier).await {
        Some(u) => u,
        None => {
            return (StatusCode::UNAUTHORIZED, Json(json!({"success":false,"error":"Invalid credentials"})))
                .into_response()
        }
    };
    let hash = match fetch_password_hash(user.id).await {
        Some(h) => h,
        None => {
            return (StatusCode::UNAUTHORIZED, Json(json!({"success":false,"error":"Invalid credentials"})))
                .into_response()
        }
    };
    if !verify_password(&password, &hash) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"success":false,"error":"Invalid credentials"})))
            .into_response();
    }
    let token = match create_session(user.id).await {
        Ok(t) => t,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success":false,"error":e})))
                .into_response()
        }
    };

    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        cfg.cookie_name, token, cfg.session_lifetime
    );
    let mut response = Json(json!({
        "success": true,
        "user": { "id": user.id, "name": user.name, "email": user.email, "role": user.role }
    }))
    .into_response();

    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub async fn logout_handler(req: Request) -> impl IntoResponse {
    let cfg = auth_config();
    let cookie_name = cfg.cookie_name.clone();
    let token = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_cookie_header(h, &cookie_name));
    if let Some(token) = token {
        let _ = delete_session(&token).await;
    }

    let cookie = format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", cookie_name);
    let mut response = Redirect::to("/").into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub async fn me_handler(axum::Extension(auth): axum::Extension<AuthUser>) -> impl IntoResponse {
    match auth.user {
        Some(user) => (StatusCode::OK, Json(json!({
            "authenticated": true,
            "user": { "id": user.id, "name": user.name, "email": user.email, "role": user.role }
        })))
            .into_response(),
        None => (StatusCode::UNAUTHORIZED, Json(json!({
            "authenticated": false, "error": "Not authenticated"
        })))
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// setpass CLI
// ---------------------------------------------------------------------------
#[allow(dead_code)]
pub async fn set_password(identifier: &str, password: &str) -> Result<(), String> {
    let cfg = auth_config();
    let pool = DB_POOL.get().ok_or("Database not configured")?;
    let hash = hash_password(password).await?;
    let p1 = cfg.placeholder(pool, 1);
    let p2 = cfg.placeholder(pool, 2);
    let sql = format!(
        "UPDATE {} SET {} = {} WHERE {} = {}",
        cfg.user_table, cfg.user_password, p1, cfg.user_identifier, p2
    );
    match pool {
        DbPool::Sqlite(c) => sqlx::query(&sql).bind(&hash).bind(identifier).execute(c).await.map(|_| ()),
        DbPool::Postgres(c) => sqlx::query(&sql).bind(&hash).bind(identifier).execute(c).await.map(|_| ()),
        DbPool::MySql(c) => sqlx::query(&sql).bind(&hash).bind(identifier).execute(c).await.map(|_| ()),
    }
    .map_err(|e| format!("Failed to update password: {}", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Page Guards
// ---------------------------------------------------------------------------

static LAYOUT_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<([A-Za-z][A-Za-z0-9_-]*)Layout\s+([^>]*?)>"#).unwrap()
});

pub struct PageGuard {
    pub auth: bool,
    pub roles: Vec<String>,
    pub guest: bool,
}

pub fn extract_page_guard(source: &str) -> Option<PageGuard> {
    let caps = LAYOUT_TAG_RE.captures(source)?;
    let props = parse_props_v7(caps.get(2)?.as_str());

    let read_bool = |key: &str| match props.get(key) {
        Some(serde_json::Value::Bool(v)) => *v,
        Some(serde_json::Value::String(v)) => v.eq_ignore_ascii_case("true"),
        _ => false,
    };

    let auth = read_bool("auth");
    let guest = read_bool("guest");
    let mut roles = Vec::new();
    if let Some(serde_json::Value::String(s)) = props.get("roles") {
        roles.extend(s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from));
    }
    if let Some(serde_json::Value::String(s)) = props.get("role") {
        roles.extend(s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from));
    }
    if auth || guest || !roles.is_empty() {
        Some(PageGuard { auth, roles, guest })
    } else {
        None
    }
}

fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("/login") && !n.contains("://") => n.into(),
        _ => "/dashboard".into(),
    }
}

pub fn check_page_guard(
    guard: &PageGuard,
    user: &Option<User>,
    current_path: &str,
    next: Option<&str>,
) -> Option<Response> {
    if guard.guest && user.is_some() {
        return Some(Redirect::to(&safe_next(next)).into_response());
    }
    if guard.auth && user.is_none() {
        return Some(Redirect::to(&format!("/login?next={}", current_path)).into_response());
    }
    if !guard.roles.is_empty() {
        let has_role = user.as_ref().map_or(false, |u| {
            guard.roles.iter().any(|r| r.eq_ignore_ascii_case(&u.role))
        });
        if !has_role {
            if user.is_none() {
                return Some(Redirect::to(&format!("/login?next={}", current_path)).into_response());
            }
            return Some(
                (StatusCode::FORBIDDEN, Html("<h1>403 Forbidden</h1><p>Insufficient permissions.</p>".to_string()))
                    .into_response(),
            );
        }
    }
    None
}