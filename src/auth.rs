use crate::component::parse_props_v7;
use crate::database::{DbPool, DB_POOL};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::{
    extract::{FromRequest, Request},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Json, Redirect, Response},
};
use regex::Regex;
use serde_json::json;
use sqlx::FromRow;
use std::sync::{LazyLock, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

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
}

// ---------------------------------------------------------------------------
// Session Secret
// ---------------------------------------------------------------------------

static SESSION_SECRET: OnceLock<String> = OnceLock::new();

pub fn init_session_secret() {
    let secret = std::env::var("SESSION_SECRET").unwrap_or_else(|_| {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let secret: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        crate::vlo_debug!("⚠️ VLO DEBUG: Generated random SESSION_SECRET (set in .env for persistence)");
        secret
    });
    SESSION_SECRET.set(secret).ok();
}

#[allow(dead_code)]
pub fn get_secret() -> &'static str {
    SESSION_SECRET
        .get()
        .map(|s| s.as_str())
        .unwrap_or("default_secret")
}

// ---------------------------------------------------------------------------
// Password Hashing (Argon2)
// ---------------------------------------------------------------------------

pub async fn hash_password(password: &str) -> Result<String, String> {
    use argon2::{
        password_hash::{PasswordHasher, SaltString},
        Argon2,
    };
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("Failed to hash password: {}", e))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::{
        password_hash::{PasswordHash, PasswordVerifier},
        Argon2,
    };
    let parsed = match PasswordHash::new(hash) {
        Ok(h) => h,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
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
    let token = generate_token();
    let expires_at = now_timestamp() + (24 * 60 * 60);

    let pool = DB_POOL.get().ok_or("Database not configured")?;

    match pool {
        DbPool::Sqlite(p) => {
            sqlx::query("INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)")
                .bind(&token)
                .bind(user_id)
                .bind(expires_at)
                .execute(p)
                .await
                .map_err(|e| format!("Failed to create session: {}", e))?;
        }
        DbPool::Postgres(p) => {
            sqlx::query("INSERT INTO sessions (token, user_id, expires_at) VALUES ($1, $2, $3)")
                .bind(&token)
                .bind(user_id)
                .bind(expires_at)
                .execute(p)
                .await
                .map_err(|e| format!("Failed to create session: {}", e))?;
        }
        DbPool::MySql(p) => {
            sqlx::query("INSERT INTO sessions (token, user_id, expires_at) VALUES (?, ?, ?)")
                .bind(&token)
                .bind(user_id)
                .bind(expires_at)
                .execute(p)
                .await
                .map_err(|e| format!("Failed to create session: {}", e))?;
        }
    }

    Ok(token)
}

pub async fn get_user_from_session(token: &str) -> Option<User> {
    let pool = DB_POOL.get()?;
    let now = now_timestamp();

    match pool {
        DbPool::Sqlite(p) => {
            sqlx::query_as::<_, User>(
                "SELECT u.id, u.name, u.email, u.role FROM users u 
                 INNER JOIN sessions s ON u.id = s.user_id 
                 WHERE s.token = ? AND s.expires_at > ?",
            )
            .bind(token)
            .bind(now)
            .fetch_optional(p)
            .await
            .ok()
            .flatten()
        }
        DbPool::Postgres(p) => {
            sqlx::query_as::<_, User>(
                "SELECT u.id, u.name, u.email, u.role FROM users u 
                 INNER JOIN sessions s ON u.id = s.user_id 
                 WHERE s.token = $1 AND s.expires_at > $2",
            )
            .bind(token)
            .bind(now)
            .fetch_optional(p)
            .await
            .ok()
            .flatten()
        }
        DbPool::MySql(p) => {
            sqlx::query_as::<_, User>(
                "SELECT u.id, u.name, u.email, u.role FROM users u 
                 INNER JOIN sessions s ON u.id = s.user_id 
                 WHERE s.token = ? AND s.expires_at > ?",
            )
            .bind(token)
            .bind(now)
            .fetch_optional(p)
            .await
            .ok()
            .flatten()
        }
    }
}

pub async fn delete_session(token: &str) -> Result<(), String> {
    let pool = DB_POOL.get().ok_or("Database not configured")?;

    match pool {
        DbPool::Sqlite(p) => {
            sqlx::query("DELETE FROM sessions WHERE token = ?")
                .bind(token)
                .execute(p)
                .await
                .map_err(|e| format!("Failed to delete session: {}", e))?;
        }
        DbPool::Postgres(p) => {
            sqlx::query("DELETE FROM sessions WHERE token = $1")
                .bind(token)
                .execute(p)
                .await
                .map_err(|e| format!("Failed to delete session: {}", e))?;
        }
        DbPool::MySql(p) => {
            sqlx::query("DELETE FROM sessions WHERE token = ?")
                .bind(token)
                .execute(p)
                .await
                .map_err(|e| format!("Failed to delete session: {}", e))?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

fn parse_cookie_header(header_value: &str) -> Option<String> {
    for part in header_value.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("vlo_session=") {
            return Some(value.to_string());
        }
    }
    None
}

pub async fn session_middleware(mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_cookie_header);

    let user = if let Some(token) = token {
        get_user_from_session(&token).await
    } else {
        None
    };

    req.extensions_mut().insert(AuthUser { user });
    next.run(req).await
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn login_handler(req: Request) -> impl IntoResponse {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let mut email = String::new();
    let mut password = String::new();

    if content_type.contains("multipart/form-data") {
        match axum::extract::Multipart::from_request(req, &()).await {
            Ok(mut multipart) => {
                while let Ok(Some(field)) = multipart.next_field().await {
                    let name = field.name().unwrap_or("").to_string();
                    let value = field.text().await.unwrap_or_default();

                    if name == "email" {
                        email = value;
                    } else if name == "password" {
                        password = value;
                    }
                }
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "error": "Invalid multipart request"
                    })),
                )
                    .into_response();
            }
        }
    } else if content_type.contains("application/json") {
        match axum::extract::Json::<serde_json::Value>::from_request(req, &()).await {
            Ok(payload) => {
                email = payload
                    .get("email")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                password = payload
                    .get("password")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "error": "Invalid JSON"
                    })),
                )
                    .into_response();
            }
        }
    } else if content_type.contains("application/x-www-form-urlencoded") {
        match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(bytes) => {
                let body_str = String::from_utf8_lossy(&bytes);
                for pair in body_str.split('&') {
                    let mut parts = pair.splitn(2, '=');
                    if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                        let decoded_value =
                            urlencoding::decode(value).unwrap_or_default().to_string();
                        if key == "email" {
                            email = decoded_value;
                        } else if key == "password" {
                            password = decoded_value;
                        }
                    }
                }
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "success": false,
                        "error": "Failed to read body"
                    })),
                )
                    .into_response();
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "error": "Unsupported content type"
            })),
        )
            .into_response();
    }

    if email.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "success": false,
                "error": "Email and password are required"
            })),
        )
            .into_response();
    }

    let pool = match DB_POOL.get() {
        Some(p) => p,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "error": "Database not configured"
                })),
            )
                .into_response();
        }
    };

    let user_result = match pool {
        DbPool::Sqlite(p) => {
            sqlx::query_as::<_, User>("SELECT id, name, email, role FROM users WHERE email = ?")
                .bind(&email)
                .fetch_optional(p)
                .await
        }
        DbPool::Postgres(p) => {
            sqlx::query_as::<_, User>("SELECT id, name, email, role FROM users WHERE email = $1")
                .bind(&email)
                .fetch_optional(p)
                .await
        }
        DbPool::MySql(p) => {
            sqlx::query_as::<_, User>("SELECT id, name, email, role FROM users WHERE email = ?")
                .bind(&email)
                .fetch_optional(p)
                .await
        }
    };

    let user = match user_result {
        Ok(Some(u)) => u,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "success": false,
                    "error": "Invalid email or password"
                })),
            )
                .into_response();
        }
    };

    let hash_result = match pool {
        DbPool::Sqlite(p) => {
            sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE id = ?")
                .bind(user.id)
                .fetch_optional(p)
                .await
        }
        DbPool::Postgres(p) => {
            sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE id = $1")
                .bind(user.id)
                .fetch_optional(p)
                .await
        }
        DbPool::MySql(p) => {
            sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE id = ?")
                .bind(user.id)
                .fetch_optional(p)
                .await
        }
    };

    let hash = match hash_result {
        Ok(Some(h)) => h,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({
                    "success": false,
                    "error": "Invalid credentials"
                })),
            )
                .into_response();
        }
    };

    if !verify_password(&password, &hash) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "success": false,
                "error": "Invalid email or password"
            })),
        )
            .into_response();
    }

    let token = match create_session(user.id).await {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "success": false,
                    "error": format!("Failed to create session: {}", e)
                })),
            )
                .into_response();
        }
    };

    let cookie_str = format!(
        "vlo_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400",
        token
    );

    let mut response = Json(json!({
        "success": true,
        "user": {
            "id": user.id,
            "name": user.name,
            "email": user.email,
            "role": user.role
        }
    }))
    .into_response();

    if let Ok(header_value) = HeaderValue::from_str(&cookie_str) {
        response
            .headers_mut()
            .append(header::SET_COOKIE, header_value);
    }

    response
}

pub async fn logout_handler(req: Request) -> impl IntoResponse {
    let token = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_cookie_header);

    if let Some(t) = token {
        let _ = delete_session(&t).await;
    }

    let cookie_str = "vlo_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0";

    let mut response = Redirect::to("/").into_response();

    if let Ok(header_value) = HeaderValue::from_str(cookie_str) {
        response
            .headers_mut()
            .append(header::SET_COOKIE, header_value);
    }

    response
}

pub async fn me_handler(axum::Extension(auth): axum::Extension<AuthUser>) -> impl IntoResponse {
    match auth.user {
        Some(user) => (
            StatusCode::OK,
            Json(json!({
                "authenticated": true,
                "user": {
                    "id": user.id,
                    "name": user.name,
                    "email": user.email,
                    "role": user.role
                }
            })),
        )
            .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "authenticated": false,
                "error": "Not authenticated"
            })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Page Guards (Auth, Roles & Guest-only via BaseLayout config)
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
    let props_str = caps.get(2)?.as_str();
    let props = parse_props_v7(props_str);

    let read_bool = |key: &str| -> bool {
        match props.get(key) {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
            _ => false,
        }
    };

    let auth = read_bool("auth");
    let guest = read_bool("guest");

    let roles = match props.get("roles") {
        Some(serde_json::Value::String(s)) => s
            .split(',')
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect(),
        _ => vec![],
    };

    if auth || guest || !roles.is_empty() {
        Some(PageGuard { auth, roles, guest })
    } else {
        None
    }
}

fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if n.starts_with('/') && !n.starts_with("/login") && !n.contains("://") =>
        {
            n.to_string()
        }
        _ => "/dashboard".to_string(),
    }
}

pub fn check_page_guard(
    guard: &PageGuard,
    user: &Option<User>,
    current_path: &str,
    next: Option<&str>,
) -> Option<Response> {
    // Guest-only pages (login/register): logged-in users are sent away instantly
    if guard.guest && user.is_some() {
        return Some(Redirect::to(&safe_next(next)).into_response());
    }

    // Protected pages: anonymous users go to login
    if guard.auth && user.is_none() {
        return Some(
            Redirect::to(&format!("/login?next={}", current_path)).into_response(),
        );
    }

    // Role-restricted pages
    if !guard.roles.is_empty() {
        let has_role = user.as_ref().map_or(false, |u| {
            guard.roles.iter().any(|r| r.eq_ignore_ascii_case(&u.role))
        });

        if !has_role {
            if user.is_none() {
                return Some(
                    Redirect::to(&format!("/login?next={}", current_path)).into_response(),
                );
            }
            return Some(
                (
                    StatusCode::FORBIDDEN,
                    Html("<h1>403 Forbidden</h1><p>Insufficient permissions.</p>".to_string()),
                )
                    .into_response(),
            );
        }
    }

    None
}