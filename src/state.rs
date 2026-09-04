use regex::Regex;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};
use serde_json::Value;

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

pub static SQL_PARAM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{{1,2}\s*([a-zA-Z0-9_-]+)\s*\}{1,2}").unwrap());

#[derive(Debug)]
pub struct CompiledTemplate {
    pub template: String,
    pub css: String,
}

pub static TEMPLATE_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<CompiledTemplate>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Development,
    Production,
}

impl AppMode {
    pub fn is_dev(self) -> bool {
        matches!(self, Self::Development)
    }
}

pub static APP_MODE: LazyLock<Mutex<AppMode>> =
    LazyLock::new(|| Mutex::new(AppMode::Development));

pub fn set_app_mode(mode: AppMode) {
    if let Ok(mut current) = APP_MODE.lock() {
        *current = mode;
    }
}

pub fn app_mode() -> AppMode {
    APP_MODE
        .lock()
        .map(|mode| *mode)
        .unwrap_or(AppMode::Development)
}

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

pub fn get_project_root() -> PathBuf {
    PROJECT_ROOT.clone()
}

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