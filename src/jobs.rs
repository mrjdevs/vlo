use crate::database::get_pool;
use crate::features::{Feature, is_enabled};
use serde_json::Value;
use std::time::Duration;
use sqlx::FromRow;

// ---------------------------------------------------------------------------
// Job Struct (Maps directly to vlo_jobs table)
// ---------------------------------------------------------------------------
#[derive(Debug, FromRow)]
struct PendingJob {
    id: i64,
    job_type: String,
    payload: String,
    attempts: i32,
    max_attempts: i32,
}

// ---------------------------------------------------------------------------
// Job Dispatching
// ---------------------------------------------------------------------------
pub async fn dispatch(job_type: &str, payload: Value, delay_seconds: Option<i64>) -> Result<(), String> {
    if !is_enabled(Feature::Jobs) {
        return Err("Jobs feature is disabled".to_string());
    }

    let pool = get_pool()?;
    let delay = delay_seconds.unwrap_or(0);
    let payload_str = payload.to_string();

    let p1 = pool.placeholder(1);
    let p2 = pool.placeholder(2);
    let run_at_sql = pool.date_add_seconds_sql(3);

    let sql = format!(
        "INSERT INTO vlo_jobs (job_type, payload, status, run_at) VALUES ({}, {}, 'pending', {})",
        p1, p2, run_at_sql
    );

    crate::db_execute!(pool, &sql, job_type, &payload_str, delay)
        .map_err(|e| format!("Failed to dispatch job: {}", e))?;

    crate::vlo_debug!("📦 [JOBS] Dispatched: {} (delay: {:?}s)", job_type, delay_seconds);
    Ok(())
}

// ---------------------------------------------------------------------------
// Background Worker
// ---------------------------------------------------------------------------
pub async fn start_worker() {
    crate::vlo_debug!("⚙️ [JOBS] Background worker started");
    
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await; // Poll every 2 seconds

        if let Err(e) = process_next_job().await {
            crate::vlo_debug!("⚠️ [JOBS] Worker error: {}", e);
        }
    }
}

async fn process_next_job() -> Result<(), String> {
    let pool = get_pool()?;
    let p1 = pool.placeholder(1);

    // 1. Fetch next pending job using the cross-DB macro
    // 🔥 FIX: Removed .await because the macro already awaits internally!
    let fetch_sql = "SELECT id, job_type, payload, attempts, max_attempts FROM vlo_jobs WHERE status = 'pending' AND run_at <= CURRENT_TIMESTAMP ORDER BY run_at ASC LIMIT 1";
    
    let job: Option<PendingJob> = crate::db_fetch_optional_as!(pool, PendingJob, fetch_sql)
        .map_err(|e| format!("Failed to fetch job: {}", e))?;

    let Some(job) = job else {
        return Ok(()); // No jobs to process
    };

    // 2. Mark as processing
    // 🔥 FIX: No .await needed here either, the macro handles it
    let update_sql = format!("UPDATE vlo_jobs SET status = 'processing', attempts = attempts + 1 WHERE id = {}", p1);
    crate::db_execute!(pool, &update_sql, job.id)
        .map_err(|e| format!("Failed to mark processing: {}", e))?;

    crate::vlo_debug!("⚡ [JOBS] Processing: {} (id: {}, attempt: {}/{})", job.job_type, job.id, job.attempts + 1, job.max_attempts);

    // 3. Parse payload and execute
    let payload: Value = serde_json::from_str(&job.payload).unwrap_or(Value::Null);
    
    match execute_job_handler(&job.job_type, payload).await {
        Ok(_) => {
            // 4. Mark as completed
            let complete_sql = format!("UPDATE vlo_jobs SET status = 'completed', last_error = NULL WHERE id = {}", p1);
            let _ = crate::db_execute!(pool, &complete_sql, job.id).map_err(|e| crate::vlo_debug!("Failed to complete: {}", e));
            crate::vlo_debug!("✅ [JOBS] Completed: {} (id: {})", job.job_type, job.id);
        }
        Err(e) => {
            // 5. Handle failure (retry or mark failed)
            let p2 = pool.placeholder(2);
            let next_attempts = job.attempts + 1;
            
            if next_attempts >= job.max_attempts {
                let fail_sql = format!("UPDATE vlo_jobs SET status = 'failed', last_error = {} WHERE id = {}", p1, p2);
                let _ = crate::db_execute!(pool, &fail_sql, &e, job.id).map_err(|err| crate::vlo_debug!("Failed to mark failed: {}", err));
                crate::vlo_debug!("❌ [JOBS] Failed permanently: {} (id: {}) - {}", job.job_type, job.id, e);
            } else {
                // Retry with 60 second backoff
                let _p3 = pool.placeholder(3);
                let retry_run_at = pool.date_add_seconds_sql(3);
                let retry_sql = format!("UPDATE vlo_jobs SET status = 'pending', last_error = {}, run_at = {} WHERE id = {}", p1, retry_run_at, p2);
                let _ = crate::db_execute!(pool, &retry_sql, &e, job.id, 60).map_err(|err| crate::vlo_debug!("Failed to retry: {}", err));
                crate::vlo_debug!("🔄 [JOBS] Retrying: {} (id: {}) in 60s - {}", job.job_type, job.id, e);
            }
        }
    }

    Ok(())
}
// ---------------------------------------------------------------------------
// Job Handler Registry
// ---------------------------------------------------------------------------
async fn execute_job_handler(job_type: &str, payload: Value) -> Result<(), String> {
    match job_type {
        "send_welcome_email" => handle_welcome_email(payload).await,
        "send_password_reset" => handle_password_reset(payload).await,
        "send_activation_email" => handle_activation_email(payload).await,
        "cleanup_expired_sessions" => handle_cleanup_sessions(payload).await,
        _ => Err(format!("Unknown job type: {}", job_type)),
    }
}

// --- Example Job Handlers ---

async fn handle_welcome_email(payload: Value) -> Result<(), String> {
    let email = payload["email"].as_str().ok_or("Missing email in payload")?;
    let subject = payload["subject"].as_str().ok_or("Missing subject in payload")?;
    let body = payload["body"].as_str().ok_or("Missing body in payload")?;
    
    crate::vlo_debug!("📧 [MAILER] Sending welcome email to: {}", email);
    crate::mailer::send_email(email, subject, body).await?;
    Ok(())
}

async fn handle_password_reset(payload: Value) -> Result<(), String> {
    let email = payload["email"].as_str().ok_or("Missing email in payload")?;
    let subject = payload["subject"].as_str().ok_or("Missing subject in payload")?;
    let body = payload["body"].as_str().ok_or("Missing body in payload")?;
    
    crate::vlo_debug!("📧 [MAILER] Sending password reset to: {}", email);
    crate::mailer::send_email(email, subject, body).await?;
    Ok(())
}

async fn handle_activation_email(payload: Value) -> Result<(), String> {
    let email = payload["email"].as_str().ok_or("Missing email in payload")?;
    let subject = payload["subject"].as_str().ok_or("Missing subject in payload")?;
    let body = payload["body"].as_str().ok_or("Missing body in payload")?;
    
    crate::vlo_debug!("📧 [MAILER] Sending activation email to: {}", email);
    crate::mailer::send_email(email, subject, body).await?;
    Ok(())
}

async fn handle_cleanup_sessions(_payload: Value) -> Result<(), String> {
    crate::vlo_debug!("🧹 [JOBS] Running session cleanup...");
    let _ = crate::auth::cleanup_expired_sessions().await;
    Ok(())
}