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

//pub static EMPTY_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
//    Regex::new(r"(?i)<([a-z1-6]+)(\s+[^>]*)?>\s*</[a-z1-6]+>").unwrap()
//});

#[derive(Debug)]
pub struct CompiledTemplate {
    pub template: String,
    pub css: String,
}

pub static TEMPLATE_CACHE: LazyLock<Mutex<HashMap<PathBuf, Arc<CompiledTemplate>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
    pub page_status: bool,
}

impl Default for RenderedPage {
    fn default() -> Self {
        Self {
            html: String::new(),
            styles: Vec::new(),
            page_status: false,
        }
    }
}

impl RenderedPage {

    pub fn insert(&mut self, key: &str, value: Value) {
        self.page_status = key == "status" || key == "action";
        // This part depends on how your current RenderedPage stores
        // template context.
    }

    pub fn has_page_status(&self) -> bool {
        self.page_status
    }
}

impl RenderedPage {
    pub fn add_style(&mut self, name: &str, css: &str) {
        let marker = format!("/* VLO:{} */", name);

        if self
            .styles
            .iter()
            .any(|style| style.contains(&marker))
        {
            return;
        }

        self.styles.push(format!("{}\n{}", marker, css));
    }
}