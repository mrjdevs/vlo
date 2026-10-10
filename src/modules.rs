use crate::state::{get_module_registry, get_project_root, LoadedModule, ModuleManifest};
use crate::api::ApiAction;
use serde_json::Value;
use std::{collections::HashMap, fs, path::{Path, PathBuf}};

pub fn init_modules() -> Result<(), String> {
    let root = get_project_root();
    let modules_dir = root.join("modules");
    if !modules_dir.exists() {
        crate::vlo_debug!("📦 No modules directory found");
        return Ok(());
    }

    let mut modules = Vec::new();
    if let Ok(entries) = fs::read_dir(&modules_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            let manifest_path = path.join("module.json");
            if !manifest_path.exists() { continue; }
            match load_module(&path, &manifest_path) {
                Ok(module) => {
                    crate::vlo_debug!(
                        "📦 Loaded module: {} v{} ({} components, {} api, {} styles)",
                        module.manifest.name,
                        module.manifest.version,
                        module.components.len(),
                        module.api_sql.len(),
                        module.styles.len()
                    );
                    modules.push(module);
                }
                Err(e) => {
                    eprintln!("⚠️ Failed to load module '{}': {}", path.display(), e);
                }
            }
        }
    }

    let modules = resolve_dependencies(modules)?;
    if let Ok(mut registry) = get_module_registry().lock() {
        *registry = modules;
    }
    Ok(())
}

fn load_module(dir: &Path, manifest_path: &Path) -> Result<LoadedModule, String> {
    let manifest_str = fs::read_to_string(manifest_path)
        .map_err(|e| format!("Cannot read module.json: {}", e))?;
    let manifest: ModuleManifest = serde_json::from_str(&manifest_str)
        .map_err(|e| format!("Invalid module.json: {}", e))?;

    let mut module = LoadedModule {
        manifest: manifest.clone(),
        path: dir.to_path_buf(),
        components: Vec::new(),
        api_sql: HashMap::new(),
        styles: Vec::new(),
        scripts: Vec::new(),
    };

    let components_dir = dir.join("components");
    if components_dir.exists() {
        if let Ok(entries) = fs::read_dir(&components_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("vlo") {
                    module.components.push(path);
                }
            }
        }
    }

    // 🔥 Load API endpoints as ApiAction (supports both strings and objects)
    let api_path = dir.join("api.vlo");
    if api_path.exists() {
        if let Ok(content) = fs::read_to_string(&api_path) {
            if let Some(block) = crate::api::extract_server_block(&content) {
                let clean = block.trim_start_matches('\u{feff}').replace('\u{a0}', " ").replace('\r', "");
                if let Ok(json) = serde_json::from_str::<Value>(&clean) {
                    if let Some(obj) = json.as_object() {
                        for (name, value) in obj {
                            if let Ok(action) = serde_json::from_value::<ApiAction>(value.clone()) {
                                module.api_sql.insert(name.clone(), action);
                            }
                        }
                    }
                }
            }
        }
    }

    let styles_dir = dir.join("styles");
    if styles_dir.exists() {
        if let Ok(entries) = fs::read_dir(&styles_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("css") {
                    if let Ok(css) = fs::read_to_string(&path) {
                        module.styles.push(css);
                    }
                }
            }
        }
    }

    let scripts_dir = dir.join("scripts");
    if scripts_dir.exists() {
        if let Ok(entries) = fs::read_dir(&scripts_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("js") {
                    if let Ok(js) = fs::read_to_string(&path) {
                        module.scripts.push(js);
                    }
                }
            }
        }
    }

    Ok(module)
}

fn resolve_dependencies(modules: Vec<LoadedModule>) -> Result<Vec<LoadedModule>, String> {
    let names: Vec<String> = modules.iter().map(|m| m.manifest.name.clone()).collect();
    for module in &modules {
        for dep in &module.manifest.dependencies {
            if dep != "base" && !names.contains(dep) {
                return Err(format!(
                    "Module '{}' depends on '{}' which is not installed",
                    module.manifest.name, dep
                ));
            }
        }
    }
    Ok(modules)
}

pub fn get_modules() -> Vec<LoadedModule> {
    get_module_registry().lock().map(|m| m.clone()).unwrap_or_default()
}

pub fn find_module_component(name: &str) -> Option<PathBuf> {
    let modules = get_modules();
    for module in modules {
        let component_path = module.path.join("components").join(format!("{}.vlo", name));
        if component_path.exists() {
            return Some(component_path);
        }
    }
    None
}

// 🔥 Returns HashMap<String, ApiAction> instead of HashMap<String, String>
pub fn get_module_api_actions() -> HashMap<String, ApiAction> {
    let modules = get_modules();
    let mut actions = HashMap::new();
    for module in modules {
        actions.extend(module.api_sql);
    }
    actions
}