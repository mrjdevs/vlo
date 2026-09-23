use regex::Regex;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, LazyLock, Mutex,
    },
};

// ---------------------------------------------------------------------------
// Regexes (compiled once, shared everywhere).
// All `{` / `}` are escaped so the regex crate does not treat them as
// repetition operators (this was the cause of the earlier startup panic).
// ---------------------------------------------------------------------------
pub static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style[^>]*>(.*?)</style>").unwrap());

pub static ELEMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<([a-zA-Z][a-zA-Z0-9-]*)(\s[^>]*)?>").unwrap());

pub static PROP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([a-zA-Z0-9_@.-]+)\s*\}\}").unwrap());

pub static CLASS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bclass\s*=\s*("([^"]*)"|'([^']*)')"#).unwrap());

pub static SLOT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<slot(?:\s+name\s*=\s*["']([^"']+)["'])?\s*(?:/>|>(.*?)</slot>|>)"#,
    )
    .unwrap()
});

// ---------------------------------------------------------------------------
// Compiled template cache
// ---------------------------------------------------------------------------
#[derive(Debug)]
pub struct CompiledTemplate {
    pub template: String,
    pub css: String,
}

pub static TEMPLATE_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<CompiledTemplate>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// Debug logging
// ---------------------------------------------------------------------------
pub static VLO_DEBUG: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("VLO_DEBUG")
        .map(|value| env_bool_from_value(&value))
        .unwrap_or(false)
});

pub fn env_bool_from_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[macro_export]
macro_rules! vlo_debug {
    ($($arg:tt)*) => {
        if *$crate::state::VLO_DEBUG {
            println!($($arg)*);
        }
    };
}

// ---------------------------------------------------------------------------
// Project root discovery
// ---------------------------------------------------------------------------
pub static PROJECT_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let mut starts = Vec::new();
    if let Ok(dir) = std::env::current_dir() {
        starts.push(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            starts.push(parent.to_path_buf());
        }
    }
    starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    for start in starts {
        let mut dir = start;
        loop {
            if dir.join("pages").exists() {
                return dir;
            }
            if !dir.pop() {
                break;
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
});

// Returns an owned PathBuf (keeps server.rs `&root` / `&build_dir` types
// matching, avoiding the earlier `&PathBuf` vs `&&Path` compile error).
pub fn get_project_root() -> PathBuf {
    PROJECT_ROOT.clone()
}

// ---------------------------------------------------------------------------
// Upload limits
// ---------------------------------------------------------------------------
pub fn max_upload_bytes() -> u64 {
    let mb = std::env::var("VLO_MAX_UPLOAD_MB")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(10); // default 10 MB
    mb.saturating_mul(1024 * 1024)
}

// ---------------------------------------------------------------------------
// App mode (Development / Production)
//
// main.rs and server.rs call `set_app_mode(...)` at startup, and
// router.rs reads it via `app_mode().is_dev()` to decide whether to inject
// the HMR reload script. Stored in an AtomicBool so reads are lock-free.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Development,
    Production,
}

impl AppMode {
    pub fn is_dev(self) -> bool {
        matches!(self, AppMode::Development)
    }
}

static IS_PRODUCTION: AtomicBool = AtomicBool::new(false);

pub fn set_app_mode(mode: AppMode) {
    IS_PRODUCTION.store(matches!(mode, AppMode::Production), Ordering::Relaxed);
}

pub fn app_mode() -> AppMode {
    if IS_PRODUCTION.load(Ordering::Relaxed) {
        AppMode::Production
    } else {
        AppMode::Development
    }
}

// ---------------------------------------------------------------------------
// Rendered page
//
// `template_context` holds the variables available to the template engine
// (query params, data-source results, etc.). `insert` forwards into it, which
// is what router.rs relies on — so it is now genuinely used (no dead code).
// ---------------------------------------------------------------------------
pub struct RenderedPage {
    pub html: String,
    pub styles: Vec<String>,
    pub template_context: HashMap<String, Value>,
}

impl Default for RenderedPage {
    fn default() -> Self {
        Self {
            html: String::new(),
            styles: Vec::new(),
            template_context: HashMap::new(),
        }
    }
}

impl RenderedPage {
    // Accepts &str / &String (via deref coercion) — router.rs passes &String.
    pub fn insert(&mut self, key: &str, value: Value) {
        self.template_context.insert(key.to_string(), value);
    }

    pub fn add_style(&mut self, name: &str, css: &str) {
        let marker = format!("/* VLO:{} */", name);
        if self.styles.iter().any(|style| style.contains(&marker)) {
            return;
        }
        self.styles.push(format!("{}\n{}", marker, css));
    }
}