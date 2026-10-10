use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feature {
    Auth,
    Database,
    Api,
    Modules,
    Email,
    Sse,
    Files,
    Flash,
    Pagination,
    Directives,
}

impl Feature {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auth" => Some(Self::Auth),
            "database" => Some(Self::Database),
            "api" => Some(Self::Api),
            "modules" => Some(Self::Modules),
            "email" => Some(Self::Email),
            "sse" => Some(Self::Sse),
            "files" => Some(Self::Files),
            "flash" => Some(Self::Flash),
            "pagination" => Some(Self::Pagination),
            "directives" => Some(Self::Directives),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Database => "database",
            Self::Api => "api",
            Self::Modules => "modules",
            Self::Email => "email",
            Self::Sse => "sse",
            Self::Files => "files",
            Self::Flash => "flash",
            Self::Pagination => "pagination",
            Self::Directives => "directives",
        }
    }
}

static ENABLED_FEATURES: OnceLock<HashSet<Feature>> = OnceLock::new();

pub fn init_features() {
    let features_str = std::env::var("VLO_FEATURES")
        .unwrap_or_else(|_| "auth,database,api,modules,email,sse,files,flash,pagination,directives".to_string());

    let features: HashSet<Feature> = features_str
        .split(',')
        .map(|s| s.trim())  
        .filter(|s| !s.is_empty()) 
        .filter_map(Feature::from_str) 
        .collect();

    ENABLED_FEATURES.set(features).ok();
    
    // Print enabled features to console on startup
    let enabled: Vec<&str> = enabled_features().iter().map(|f| f.name()).collect();
    if enabled.is_empty() {
        crate::vlo_debug!("⚙️  [FEATURES] No optional features enabled (Core only)");
    } else {
        crate::vlo_debug!("⚙️  [FEATURES] Enabled: {}", enabled.join(", "));
    }
}
pub fn is_enabled(feature: Feature) -> bool {
    ENABLED_FEATURES
        .get()
        .map(|f| f.contains(&feature))
        .unwrap_or(false)
}

pub fn enabled_features() -> &'static HashSet<Feature> {
    ENABLED_FEATURES.get().expect("Features not initialized")
}