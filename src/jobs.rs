use crate::database::get_pool;
use crate::features::{Feature, is_enabled};
use serde_json::Value;
use std::time::Duration;
use sqlx::FromRow;

// ---------------------------------------------------------------------------
// Job Struct
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
// 🔥 AUTO-MIGRATION: ensures vlo_jobs and vlo_email_rules exist
// ---------------------------------------------------------------------------
pub async fn ensure_schema() -> Result<(), String> {
    let pool = get_pool()?;

    // Enhanced jobs table (adds event_type + dedup_key vs. original)
    crate::schema::ensure_table(pool, "vlo_jobs", &[
        crate::schema::ColumnDef::new("id", "INTEGER").primary_key(),
        crate::schema::ColumnDef::new("job_type", "TEXT").not_null(),
        crate::schema::ColumnDef::new("payload", "TEXT").not_null(),
        crate::schema::ColumnDef::new("status", "TEXT").default("'pending'"),
        crate::schema::ColumnDef::new("attempts", "INTEGER").default("0"),
        crate::schema::ColumnDef::new("max_attempts", "INTEGER").default("3"),
        crate::schema::ColumnDef::new("last_error", "TEXT"),
        crate::schema::ColumnDef::new("run_at", "TIMESTAMP").default("CURRENT_TIMESTAMP"),
        crate::schema::ColumnDef::new("event_type", "TEXT"),
        crate::schema::ColumnDef::new("dedup_key", "TEXT"),
        crate::schema::ColumnDef::new("created_at", "TIMESTAMP").default("CURRENT_TIMESTAMP"),
    ]).await?;

    // Unique index for dedup (NULLs allowed multiple times on SQLite/PG/MySQL)
    let _ = pool.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_vlo_jobs_dedup ON vlo_jobs(dedup_key)"
    ).await;

    // Email dispatch rules
    crate::schema::ensure_table(pool, "vlo_email_rules", &[
        crate::schema::ColumnDef::new("id", "INTEGER").primary_key(),
        crate::schema::ColumnDef::new("event_type", "TEXT").not_null(),
        crate::schema::ColumnDef::new("template", "TEXT").not_null(),
        crate::schema::ColumnDef::new("recipient", "TEXT").not_null(),
        crate::schema::ColumnDef::new("condition_path", "TEXT"),
        crate::schema::ColumnDef::new("condition_op", "TEXT"),
        crate::schema::ColumnDef::new("condition_val", "TEXT"),
        crate::schema::ColumnDef::new("delay_minutes", "INTEGER").default("0"),
        crate::schema::ColumnDef::new("dedup_minutes", "INTEGER").default("-1"), // -1 = fire once ever; 0 = every time; N = one per N minutes
        crate::schema::ColumnDef::new("enabled", "INTEGER").default("1"),
        crate::schema::ColumnDef::new("created_at", "TIMESTAMP").default("CURRENT_TIMESTAMP"),
    ]).await?;

    crate::vlo_debug!("✅ [JOBS] Schema ready (vlo_jobs, vlo_email_rules)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch (low-level — used by auth.rs and other Rust callers)
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
// 🔥 AUTO-EMIT: dispatches emails based on vlo_email_rules
// Called automatically by api.rs after every mutating action succeeds.
// ---------------------------------------------------------------------------
pub async fn auto_emit_events(
    pool: &crate::database::DbPool,
    event_type: &str,
    context: &Value,
    user_id: Option<i64>,
) -> Result<(), String> {
    // Fetch all enabled rules for this event
    let rules_sql = format!(
        "SELECT template, recipient, condition_path, condition_op, condition_val, delay_minutes, dedup_minutes \
         FROM vlo_email_rules WHERE event_type = '{}' AND enabled = 1",
        event_type.replace('\'', "''")
    );
    let rules = pool.fetch_all_json(&rules_sql).await.map_err(|e| e.to_string())?;

    if rules.is_empty() {
        return Ok(());
    }

    // Extract row id from context for dedup (any of these keys)
    let row_id = context.get("id").and_then(|v| v.as_i64()).unwrap_or(0);

    for rule in rules {
        let template   = rule.get("template").and_then(|v| v.as_str()).unwrap_or("");
        let recipient  = rule.get("recipient").and_then(|v| v.as_str()).unwrap_or("");
        let delay      = rule.get("delay_minutes").and_then(|v| v.as_i64()).unwrap_or(0);
        let dedup_min  = rule.get("dedup_minutes").and_then(|v| v.as_i64()).unwrap_or(-1);

        // Resolve recipient (JSON path `$.customer_email` or static `admin@site.com`)
        let to = if recipient.starts_with("$.") {
            let path = &recipient[2..];
            context.get(path).and_then(|v| v.as_str()).unwrap_or("").to_string()
        } else {
            recipient.to_string()
        };
        if to.is_empty() {
            crate::vlo_debug!("⚠️ [JOBS] Skipping rule '{}' — recipient resolved empty for event '{}'", template, event_type);
            continue;
        }

        // Evaluate condition (if any)
        if let (Some(cond_path), Some(op), Some(cond_val)) = (
            rule.get("condition_path").and_then(|v| v.as_str()),
            rule.get("condition_op").and_then(|v| v.as_str()),
            rule.get("condition_val").and_then(|v| v.as_str()),
        ) {
            let actual = if cond_path.starts_with("$.") {
                context.get(&cond_path[2..]).cloned().unwrap_or(Value::Null)
            } else {
                context.get(cond_path).cloned().unwrap_or(Value::Null)
            };
            if !eval_condition(&actual, op, cond_val) {
                continue;
            }
        }

        // Build dedup key
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let dedup_key: Option<String> = match dedup_min {
            -1 => Some(format!("{}:{}:{}:{}", event_type, template, user_id.unwrap_or(0), row_id)),
            0  => None,
            n if n > 0 => {
                let window_secs = n * 60;
                Some(format!("{}:{}:{}:{}:{}", event_type, template, user_id.unwrap_or(0), row_id, now_secs / window_secs))
            }
            _ => None,
        };

        // Build payload
        let payload = serde_json::json!({
            "to": to,
            "template": template,
            "vars": context,
        });
        let payload_str = payload.to_string().replace('\'', "''");

        // Insert with dedup (UNIQUE INDEX catches dupes → we ignore the error)
        let (dedup_col, dedup_val) = match &dedup_key {
            Some(k) => (", dedup_key", format!(", '{}'", k.replace('\'', "''"))),
            None => ("", String::new()),
        };

        let insert_sql = format!(
            "INSERT INTO vlo_jobs (job_type, event_type, payload{}, status, run_at) \
             VALUES ('send_templated_email', '{}', '{}'{} , 'pending', \
             datetime(CURRENT_TIMESTAMP, '+' || {} || ' minutes'))",
            dedup_col,
            event_type.replace('\'', "''"),
            payload_str,
            dedup_val,
            delay
        );

        match pool.execute(&insert_sql).await {
            Ok(_) => {
                crate::vlo_debug!(
                    "📧 [JOBS] Queued email '{}' for '{}' (event: {})",
                    template, to, event_type
                );
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("UNIQUE") || msg.contains("Duplicate") || msg.contains("unique") {
                    crate::vlo_debug!("🔁 [JOBS] Dedup hit — skipping duplicate '{}' for '{}'", template, to);
                } else {
                    crate::vlo_debug!("⚠️ [JOBS] Failed to queue '{}' for '{}': {}", template, to, msg);
                }
            }
        }
    }

    Ok(())
}

fn eval_condition(actual: &Value, op: &str, expected: &str) -> bool {
    // Numeric comparison first
    if let (Some(a), Ok(e)) = (actual.as_f64(), expected.parse::<f64>()) {
        return match op {
            ">"  => a > e,
            ">=" => a >= e,
            "<"  => a < e,
            "<=" => a <= e,
            "==" => (a - e).abs() < f64::EPSILON,
            "!=" => (a - e).abs() >= f64::EPSILON,
            _ => false,
        };
    }
    // String comparison
    if let Some(a) = actual.as_str() {
        return match op {
            "==" => a == expected,
            "!=" => a != expected,
            _ => false,
        };
    }
    false
}

// ---------------------------------------------------------------------------
// Background Worker (unchanged)
// ---------------------------------------------------------------------------
pub async fn start_worker() {
    crate::vlo_debug!("⚙️ [JOBS] Background worker started");
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Err(e) = process_next_job().await {
            crate::vlo_debug!("⚠️ [JOBS] Worker error: {}", e);
        }
    }
}

async fn process_next_job() -> Result<(), String> {
    let pool = get_pool()?;
    let p1 = pool.placeholder(1);

    let fetch_sql = "SELECT id, job_type, payload, attempts, max_attempts FROM vlo_jobs WHERE status = 'pending' AND run_at <= CURRENT_TIMESTAMP ORDER BY run_at ASC LIMIT 1";
    let job: Option<PendingJob> = crate::db_fetch_optional_as!(pool, PendingJob, fetch_sql)
        .map_err(|e| format!("Failed to fetch job: {}", e))?;

    let Some(job) = job else { return Ok(()); };

    let update_sql = format!("UPDATE vlo_jobs SET status = 'processing', attempts = attempts + 1 WHERE id = {}", p1);
    crate::db_execute!(pool, &update_sql, job.id)
        .map_err(|e| format!("Failed to mark processing: {}", e))?;

    crate::vlo_debug!("⚡ [JOBS] Processing: {} (id: {}, attempt: {}/{})", job.job_type, job.id, job.attempts + 1, job.max_attempts);

    let payload: Value = serde_json::from_str(&job.payload).unwrap_or(Value::Null);

    match execute_job_handler(&job.job_type, payload).await {
        Ok(_) => {
            let complete_sql = format!("UPDATE vlo_jobs SET status = 'completed', last_error = NULL WHERE id = {}", p1);
            let _ = crate::db_execute!(pool, &complete_sql, job.id);
            crate::vlo_debug!("✅ [JOBS] Completed: {} (id: {})", job.job_type, job.id);
        }
        Err(e) => {
            let p2 = pool.placeholder(2);
            let next_attempts = job.attempts + 1;
            if next_attempts >= job.max_attempts {
                let fail_sql = format!("UPDATE vlo_jobs SET status = 'failed', last_error = {} WHERE id = {}", p1, p2);
                let _ = crate::db_execute!(pool, &fail_sql, &e, job.id);
                crate::vlo_debug!("❌ [JOBS] Failed permanently: {} (id: {}) - {}", job.job_type, job.id, e);
            } else {
                let retry_run_at = pool.date_add_seconds_sql(3);
                let retry_sql = format!("UPDATE vlo_jobs SET status = 'pending', last_error = {}, run_at = {} WHERE id = {}", p1, retry_run_at, p2);
                let _ = crate::db_execute!(pool, &retry_sql, &e, job.id, 60);
                crate::vlo_debug!("🔄 [JOBS] Retrying: {} (id: {}) in 60s - {}", job.job_type, job.id, e);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handler Registry (unchanged)
// ---------------------------------------------------------------------------
async fn execute_job_handler(job_type: &str, payload: Value) -> Result<(), String> {
    match job_type {
        "send_email"               => handle_generic_email(payload).await,
        "send_templated_email"     => handle_templated_email(payload).await,
        "send_welcome_email"       => handle_welcome_email(payload).await,
        "send_password_reset"      => handle_password_reset(payload).await,
        "send_activation_email"    => handle_activation_email(payload).await,
        "cleanup_expired_sessions" => handle_cleanup_sessions(payload).await,
        _ => Err(format!("Unknown job type: {}", job_type)),
    }
}

async fn handle_generic_email(payload: Value) -> Result<(), String> {
    let to = payload.get("to").and_then(|v| v.as_str()).ok_or("Missing 'to'")?;
    let subject = payload.get("subject").and_then(|v| v.as_str()).ok_or("Missing 'subject'")?;
    let body = payload.get("body").and_then(|v| v.as_str()).ok_or("Missing 'body'")?;
    crate::vlo_debug!("📧 [MAILER] Generic email → {}", to);
    crate::mailer::send_email(to, subject, body).await
}

async fn handle_templated_email(payload: Value) -> Result<(), String> {
    let to = payload.get("to").and_then(|v| v.as_str()).ok_or("Missing 'to'")?;
    let template_name = payload.get("template").and_then(|v| v.as_str()).ok_or("Missing 'template'")?;

    let vars: std::collections::HashMap<String, serde_json::Value> = payload
        .get("vars")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    let template = crate::mailer::render_email_template(template_name, &vars)
        .ok_or_else(|| format!("Template '{}' not found", template_name))?;

    // Skip empty body (condition-false case rendered nothing)
    if template.body.trim().is_empty() {
        crate::vlo_debug!("📧 [MAILER] Skipping empty template '{}' for {}", template_name, to);
        return Ok(());
    }

    crate::vlo_debug!("📧 [MAILER] Templated email → {} (template: {})", to, template_name);
    crate::mailer::send_email(to, &template.subject, &template.body).await
}

async fn handle_welcome_email(payload: Value) -> Result<(), String> {
    let email = payload["email"].as_str().ok_or("Missing email")?;
    let subject = payload["subject"].as_str().ok_or("Missing subject")?;
    let body = payload["body"].as_str().ok_or("Missing body")?;
    crate::mailer::send_email(email, subject, body).await?;
    Ok(())
}

async fn handle_password_reset(payload: Value) -> Result<(), String> {
    let email = payload["email"].as_str().ok_or("Missing email")?;
    let subject = payload["subject"].as_str().ok_or("Missing subject")?;
    let body = payload["body"].as_str().ok_or("Missing body")?;
    crate::mailer::send_email(email, subject, body).await?;
    Ok(())
}

async fn handle_activation_email(payload: Value) -> Result<(), String> {
    let email = payload["email"].as_str().ok_or("Missing email")?;
    let subject = payload["subject"].as_str().ok_or("Missing subject")?;
    let body = payload["body"].as_str().ok_or("Missing body")?;
    crate::mailer::send_email(email, subject, body).await?;
    Ok(())
}

async fn handle_cleanup_sessions(_: Value) -> Result<(), String> {
    let _ = crate::auth::cleanup_expired_sessions().await;
    Ok(())
}