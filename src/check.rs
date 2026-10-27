// ─────────────────────────────────────────────────────────────
// check.rs — static project validator (`vlo check`)
//
// Catches failures that otherwise surface silently: unknown components
// (rendered as an HTML comment), data-source URLs that resolve to actions
// nobody defined, malformed API JSON, unbalanced template markers and
// module exports that point at missing files.
// ─────────────────────────────────────────────────────────────
use crate::api::{sanitize_json_multiline, ApiAction};
use crate::state::{get_project_root, ModuleManifest};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

struct Diagnostic {
    file: String,
    line: usize,
    severity: Severity,
    message: String,
}

/// Diagnostic anchored to a byte offset in the file being scanned.
struct Local {
    pos: usize,
    severity: Severity,
    message: String,
}

struct Region {
    start: usize,
    end: usize,
}

static COMPONENT_OPEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<([A-Z][A-Za-z0-9]*)").unwrap());
static COMPONENT_CLOSE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</([A-Z][A-Za-z0-9]*)>").unwrap());
static ANY_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<([A-Za-z][A-Za-z0-9]*)").unwrap());
static INCLUDE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\{include\s+["']([^"']+)["']"#).unwrap());
static DATA_SOURCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(data-source|source)\s*=\s*["']([^"']*)["']"#).unwrap());

pub fn run(json: bool, strict: bool) -> Result<(), String> {
    let root = get_project_root();
    if !root.join("pages").is_dir() {
        return Err(format!("no pages/ directory found in {}", root.display()));
    }

    let mut checker = Checker::new(root);
    checker.check_modules();
    checker.check_page_api();
    checker.check_routes();
    checker.check_templates();
    checker.report(json, strict);
    Ok(())
}

struct Checker {
    root: PathBuf,
    diagnostics: Vec<Diagnostic>,
    templates: usize,
    api_files: usize,
    modules: usize,
    api_actions: HashMap<String, ApiAction>,
    api_origin: HashMap<String, String>,
    module_names: HashSet<String>,
    /// lowercase stem -> exact on-disk stems (case-mismatch detection)
    component_names: HashMap<String, Vec<String>>,
}

impl Checker {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            diagnostics: Vec::new(),
            templates: 0,
            api_files: 0,
            modules: 0,
            api_actions: HashMap::new(),
            api_origin: HashMap::new(),
            module_names: HashSet::new(),
            component_names: HashMap::new(),
        }
    }

    // ── reporting ────────────────────────────────────────────────────

    fn add_direct(&mut self, file: &str, line: usize, severity: Severity, message: String) {
        self.diagnostics.push(Diagnostic {
            file: file.to_string(),
            line: line.max(1),
            severity,
            message,
        });
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn report(&mut self, json: bool, strict: bool) {
        self.diagnostics.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.line.cmp(&b.line))
                .then(severity_rank(a.severity).cmp(&severity_rank(b.severity)))
                .then(a.message.cmp(&b.message))
        });

        let errors = self
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        let warnings = self.diagnostics.len() - errors;
        let failed = errors > 0 || (strict && warnings > 0);

        if json {
            let diagnostics: Vec<Value> = self
                .diagnostics
                .iter()
                .map(|d| {
                    serde_json::json!({
                        "file": d.file,
                        "line": d.line,
                        "severity": d.severity.as_str(),
                        "message": d.message,
                    })
                })
                .collect();
            let payload = serde_json::json!({
                "success": !failed,
                "templates": self.templates,
                "api_files": self.api_files,
                "modules": self.modules,
                "errors": errors,
                "warnings": warnings,
                "diagnostics": diagnostics,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        } else {
            println!(
                "⚡ VLO check — {} templates, {} API files, {} modules",
                self.templates, self.api_files, self.modules
            );
            println!("{}", "─".repeat(64));

            if self.diagnostics.is_empty() {
                println!("✅ No issues found");
            }
            for diagnostic in &self.diagnostics {
                let icon = match diagnostic.severity {
                    Severity::Error => "✖",
                    Severity::Warning => "⚠",
                };
                println!(
                    "{} {}:{} — {}",
                    icon, diagnostic.file, diagnostic.line, diagnostic.message
                );
            }
            if !self.diagnostics.is_empty() {
                println!();
                println!(
                    "{} {} error{}, {} warning{}",
                    if failed { "❌" } else { "✅" },
                    errors,
                    plural(errors),
                    warnings,
                    plural(warnings)
                );
            }
        }

        if failed {
            std::process::exit(1);
        }
    }

    // ── modules ──────────────────────────────────────────────────────

    fn check_modules(&mut self) {
        let modules_dir = self.root.join("modules");
        if !modules_dir.is_dir() {
            return;
        }

        let mut dirs: Vec<PathBuf> = match fs::read_dir(&modules_dir) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect(),
            Err(_) => return,
        };
        dirs.sort();

        let mut parsed: Vec<(PathBuf, ModuleManifest, String)> = Vec::new();
        for dir in dirs {
            self.modules += 1;
            let manifest_path = dir.join("module.json");
            let rel = self.rel(&manifest_path);

            if !manifest_path.exists() {
                self.add_direct(
                    &rel,
                    1,
                    Severity::Warning,
                    "module directory has no module.json".to_string(),
                );
                continue;
            }

            let source = match fs::read_to_string(&manifest_path) {
                Ok(source) => source,
                Err(error) => {
                    self.add_direct(
                        &rel,
                        1,
                        Severity::Error,
                        format!("cannot read module.json: {}", error),
                    );
                    continue;
                }
            };
            let value: Value = match serde_json::from_str(&source) {
                Ok(value) => value,
                Err(error) => {
                    self.add_direct(
                        &rel,
                        error.line(),
                        Severity::Error,
                        format!("invalid module.json: {}", error),
                    );
                    continue;
                }
            };
            let manifest: ModuleManifest = match serde_json::from_value(value) {
                Ok(manifest) => manifest,
                Err(error) => {
                    self.add_direct(
                        &rel,
                        1,
                        Severity::Error,
                        format!("invalid module manifest: {}", error),
                    );
                    continue;
                }
            };

            if !self.module_names.insert(manifest.name.clone()) {
                self.add_direct(
                    &rel,
                    1,
                    Severity::Error,
                    format!("duplicate module name '{}'", manifest.name),
                );
            }

            self.check_manifest_exports(&dir, &source, &manifest);

            let api_path = dir.join("api.vlo");
            if api_path.exists() {
                let names = self.load_api_file(&api_path, "");
                for name in &manifest.exports.api {
                    if !names.contains(name) {
                        self.add_direct(
                            &rel,
                            line_of_needle(&source, name),
                            Severity::Error,
                            format!(
                                "exports API action '{}' but api.vlo does not define it",
                                name
                            ),
                        );
                    }
                }
            } else if !manifest.exports.api.is_empty() {
                self.add_direct(
                    &rel,
                    1,
                    Severity::Error,
                    "exports API actions but the module has no api.vlo".to_string(),
                );
            }

            parsed.push((dir, manifest, source));
        }

        for (dir, manifest, source) in parsed {
            for dependency in &manifest.dependencies {
                if dependency != "base" && !self.module_names.contains(dependency) {
                    let rel = self.rel(&dir.join("module.json"));
                    self.add_direct(
                        &rel,
                        line_of_needle(&source, dependency),
                        Severity::Error,
                        format!("depends on '{}' which is not installed", dependency),
                    );
                }
            }
        }
    }

    fn check_manifest_exports(&mut self, dir: &Path, source: &str, manifest: &ModuleManifest) {
        let rel = self.rel(&dir.join("module.json"));
        let mut exported = HashSet::new();

        for component in &manifest.exports.components {
            let name = component.trim_end_matches(".vlo");
            exported.insert(name.to_string());
            if exact_vlo(&dir.join("components"), name).is_none() {
                self.add_direct(
                    &rel,
                    line_of_needle(source, component),
                    Severity::Error,
                    format!(
                        "exports component '{}' but components/{}.vlo is missing",
                        component, name
                    ),
                );
            }
        }

        if let Ok(entries) = fs::read_dir(dir.join("components")) {
            let mut stems: Vec<String> = entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("vlo") {
                        path.file_stem()
                            .and_then(|stem| stem.to_str())
                            .map(|stem| stem.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            stems.sort();
            for stem in stems {
                if !exported.contains(&stem) {
                    self.add_direct(
                        &rel,
                        1,
                        Severity::Warning,
                        format!(
                            "component '{}' is not listed in exports.components",
                            stem
                        ),
                    );
                }
            }
        }

        for style in &manifest.exports.styles {
            if !dir.join("styles").join(style).exists() {
                self.add_direct(
                    &rel,
                    line_of_needle(source, style),
                    Severity::Error,
                    format!("exports style '{}' but styles/{} is missing", style, style),
                );
            }
        }

        if let Some(scripts) = &manifest.exports.scripts {
            for script in scripts {
                if !dir.join("scripts").join(script).exists() {
                    self.add_direct(
                        &rel,
                        line_of_needle(source, script),
                        Severity::Error,
                        format!(
                            "exports script '{}' but scripts/{} is missing",
                            script, script
                        ),
                    );
                }
            }
        }
    }

    // ── API files ────────────────────────────────────────────────────

    /// Parses one `<script server>` API file, registering every action.
    /// Returns the action names defined in this file (unprefixed).
    fn load_api_file(&mut self, path: &Path, prefix: &str) -> Vec<String> {
        self.api_files += 1;
        let rel = self.rel(path);

        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                self.add_direct(
                    &rel,
                    1,
                    Severity::Error,
                    format!("cannot read API file: {}", error),
                );
                return Vec::new();
            }
        };
        let Some(tag_pos) = content.find("<script server>") else {
            self.add_direct(
                &rel,
                1,
                Severity::Error,
                "missing <script server> block".to_string(),
            );
            return Vec::new();
        };
        let block_start = tag_pos + "<script server>".len();
        let Some(block_len) = content[block_start..].find("</script>") else {
            self.add_direct(
                &rel,
                line_at(&content, tag_pos),
                Severity::Error,
                "unterminated <script server> block".to_string(),
            );
            return Vec::new();
        };
        let raw = &content[block_start..block_start + block_len];
        let base_line = line_at(&content, block_start);

        let clean = raw
            .trim_start_matches('\u{feff}')
            .replace('\u{a0}', " ")
            .replace('\r', "");
        let sanitized = sanitize_json_multiline(&clean);

        let mut seen: HashMap<String, usize> = HashMap::new();
        for (key, pos) in top_level_keys(raw) {
            let line = base_line + raw[..pos].matches('\n').count();
            if let Some(previous) = seen.insert(key.clone(), line) {
                self.add_direct(
                    &rel,
                    line,
                    Severity::Error,
                    format!(
                        "duplicate action '{}' (first defined on line {})",
                        key, previous
                    ),
                );
            }
        }

        let json: Value = match serde_json::from_str(&sanitized) {
            Ok(json) => json,
            Err(error) => {
                self.add_direct(
                    &rel,
                    base_line + error.line().saturating_sub(1),
                    Severity::Error,
                    format!("invalid JSON in server block: {}", error),
                );
                let mut keys: Vec<String> = seen.into_keys().collect();
                keys.sort();
                return keys;
            }
        };
        let Some(object) = json.as_object() else {
            self.add_direct(
                &rel,
                base_line,
                Severity::Error,
                "server block must be a JSON object".to_string(),
            );
            return Vec::new();
        };

        let mut names = Vec::new();
        for (name, value) in object {
            let line = seen.get(name).copied().unwrap_or(base_line);
            match serde_json::from_value::<ApiAction>(value.clone()) {
                Ok(action) => {
                    names.push(name.clone());
                    let action_name = format!("{}{}", prefix, name);
                    if let Some(previous) = self.api_origin.insert(action_name.clone(), rel.clone())
                    {
                        if previous != rel {
                            self.add_direct(
                                &rel,
                                line,
                                Severity::Warning,
                                format!(
                                    "action '{}' is also defined in {}; the last definition wins",
                                    action_name, previous
                                ),
                            );
                        }
                    }
                    self.api_actions.insert(action_name, action);
                }
                Err(error) => {
                    self.add_direct(
                        &rel,
                        line,
                        Severity::Error,
                        format!("invalid action '{}': {}", name, error),
                    );
                }
            }
        }
        names
    }

    fn check_page_api(&mut self) {
        let api_dir = self.root.join("pages/api");
        if !api_dir.is_dir() {
            return;
        }

        let main = api_dir.join("api.vlo");
        if main.exists() {
            self.load_api_file(&main, "");
        }

        let mut dirs: Vec<PathBuf> = match fs::read_dir(&api_dir) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect(),
            Err(_) => return,
        };
        dirs.sort();

        for dir in dirs {
            let namespace = dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .to_string();
            let file = dir.join("api.vlo");
            if file.exists() {
                self.load_api_file(&file, &format!("{}/", namespace));
            }

            // The runtime only reads pages/api/<ns>/api.vlo. Deeper nesting
            // parses fine but the actions are never registered.
            if let Ok(entries) = fs::read_dir(&dir) {
                let mut nested: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect();
                nested.sort();
                for nested_dir in nested {
                    let nested_file = nested_dir.join("api.vlo");
                    if nested_file.exists() {
                        let rel = self.rel(&nested_file);
                        self.add_direct(
                            &rel,
                            1,
                            Severity::Warning,
                            "nested API namespace is not loaded (only pages/api/<ns>/api.vlo is)"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    // ── pages ────────────────────────────────────────────────────────

    fn check_routes(&mut self) {
        let pages_dir = self.root.join("pages");
        let mut routes: HashMap<String, String> = HashMap::new();
        let mut files = Vec::new();
        collect_vlo(&pages_dir, Some("api"), &mut files);
        files.sort();

        for path in files {
            let rel = path
                .strip_prefix(&pages_dir)
                .unwrap_or(&path)
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let route = if rel == "index" {
                "/".to_string()
            } else if let Some(stripped) = rel.strip_suffix("/index") {
                format!("/{}", stripped)
            } else {
                format!("/{}", rel)
            };

            let current = self.rel(&path);
            if let Some(previous) = routes.insert(route.clone(), current.clone()) {
                self.add_direct(
                    &current,
                    1,
                    Severity::Error,
                    format!("route '{}' is also handled by {}", route, previous),
                );
            }
        }
    }

    // ── templates ────────────────────────────────────────────────────

    fn check_templates(&mut self) {
        self.collect_component_names();
        let mut files = Vec::new();
        collect_vlo(&self.root.join("pages"), Some("api"), &mut files);
        collect_vlo(&self.root.join("layouts"), None, &mut files);
        collect_vlo(&self.root.join("components"), None, &mut files);
        collect_vlo(&self.root.join("partials"), None, &mut files);
        if let Ok(entries) = fs::read_dir(self.root.join("modules")) {
            for entry in entries.flatten() {
                collect_vlo(&entry.path().join("components"), None, &mut files);
            }
        }
        files.sort();
        files.dedup();

        for path in files {
            self.check_template_file(&path);
        }
    }

    fn collect_component_names(&mut self) {
        collect_component_stems(&self.root.join("components"), &mut self.component_names);
        collect_component_stems(&self.root.join("layouts"), &mut self.component_names);
        if let Ok(entries) = fs::read_dir(self.root.join("modules")) {
            for entry in entries.flatten() {
                collect_component_stems(
                    &entry.path().join("components"),
                    &mut self.component_names,
                );
            }
        }
    }

    fn check_template_file(&mut self, path: &Path) {
        let rel = self.rel(path);
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                self.add_direct(
                    &rel,
                    1,
                    Severity::Error,
                    format!("cannot read template: {}", error),
                );
                return;
            }
        };
        self.templates += 1;

        let masked = masked_regions(&source);
        let ignored: HashSet<usize> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("vlo-check-ignore"))
            .map(|(index, _)| index + 1)
            .collect();

        let mut locals = Vec::new();
        check_markers(&source, &masked, &mut locals);
        self.check_component_tags(&source, &masked, &mut locals);
        check_includes(&self.root, &source, &masked, &mut locals);
        self.check_data_sources(&source, &masked, &mut locals);

        for local in locals {
            let line = line_at(&source, local.pos);
            if ignored.contains(&line) {
                continue;
            }
            self.add_direct(&rel, line, local.severity, local.message);
        }
    }

    fn resolve_component(&self, name: &str) -> Option<PathBuf> {
        if let Some(path) = exact_vlo(&self.root.join("layouts"), name) {
            return Some(path);
        }
        let mut layout_dirs = Vec::new();
        if let Ok(entries) = fs::read_dir(self.root.join("layouts")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    layout_dirs.push(path);
                }
            }
        }
        layout_dirs.sort();
        for dir in layout_dirs {
            if let Some(path) = exact_vlo(&dir, name) {
                return Some(path);
            }
        }
        if let Some(path) = exact_vlo(&self.root.join("components"), name) {
            return Some(path);
        }
        if let Ok(entries) = fs::read_dir(self.root.join("modules")) {
            let mut module_dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            module_dirs.sort();
            for dir in module_dirs {
                if let Some(path) = exact_vlo(&dir.join("components"), name) {
                    return Some(path);
                }
            }
        }
        None
    }

    fn check_component_tags(&self, source: &str, masked: &[Region], out: &mut Vec<Local>) {
        let mut events: Vec<(usize, bool, String)> = Vec::new();

        for captures in COMPONENT_OPEN_RE.captures_iter(source) {
            let full = captures.get(0).unwrap();
            if is_masked(masked, full.start()) {
                continue;
            }
            let name = captures.get(1).unwrap().as_str();
            let Some(open_end) = find_opening_end(source, full.end()) else {
                continue;
            };
            let self_closing = source[..open_end].trim_end().ends_with('/');

            if name != "BaseLayout" && self.resolve_component(name).is_none() {
                out.push(Local {
                    pos: full.start(),
                    severity: Severity::Error,
                    message: format!(
                        "unknown component <{}> — no layouts/{}.vlo, components/{}.vlo or module component",
                        name, name, name
                    ),
                });
            }
            if !self_closing {
                events.push((full.start(), false, name.to_string()));
            }
        }

        for captures in COMPONENT_CLOSE_RE.captures_iter(source) {
            let full = captures.get(0).unwrap();
            if is_masked(masked, full.start()) {
                continue;
            }
            events.push((
                full.start(),
                true,
                captures.get(1).unwrap().as_str().to_string(),
            ));
        }

        events.sort_by_key(|(pos, _, _)| *pos);
        let mut stack: Vec<(String, usize)> = Vec::new();
        for (pos, is_close, name) in events {
            if !is_close {
                stack.push((name, pos));
                continue;
            }
            match stack.iter().rposition(|(open, _)| *open == name) {
                Some(index) => {
                    while stack.len() > index + 1 {
                        let (inner, inner_pos) = stack.pop().unwrap();
                        out.push(Local {
                            pos: inner_pos,
                            severity: Severity::Error,
                            message: format!("component <{}> is not closed", inner),
                        });
                    }
                    stack.pop();
                }
                None => out.push(Local {
                    pos,
                    severity: Severity::Error,
                    message: format!("closing tag </{}> has no matching opening tag", name),
                }),
            }
        }
        for (name, pos) in stack {
            out.push(Local {
                pos,
                severity: Severity::Error,
                message: format!("component <{}> is not closed", name),
            });
        }

        // Case-sensitive tag typos: `<docsLayout>` silently renders as a
        // custom HTML element instead of the DocsLayout component.
        for captures in ANY_TAG_RE.captures_iter(source) {
            let full = captures.get(0).unwrap();
            if is_masked(masked, full.start()) {
                continue;
            }
            let name = captures.get(1).unwrap().as_str();
            if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                continue;
            }
            let Some(exacts) = self.component_names.get(&name.to_ascii_lowercase()) else {
                continue;
            };
            if let Some(exact) = exacts
                .iter()
                .find(|exact| exact.chars().skip(1).any(|c| c.is_ascii_uppercase()))
            {
                out.push(Local {
                    pos: full.start(),
                    severity: Severity::Warning,
                    message: format!(
                        "tag <{}> is case-sensitive; did you mean <{}>?",
                        name, exact
                    ),
                });
            }
        }
    }

    fn check_data_sources(&self, source: &str, masked: &[Region], out: &mut Vec<Local>) {
        for captures in DATA_SOURCE_RE.captures_iter(source) {
            let full = captures.get(0).unwrap();
            if is_masked(masked, full.start()) {
                continue;
            }
            let attribute = captures.get(1).unwrap().as_str().to_ascii_lowercase();
            let raw = captures.get(2).unwrap().as_str();
            if raw.trim().is_empty() {
                continue;
            }
            // Plain `source` also appears on media elements; only API URLs
            // are meant for module components.
            if attribute == "source" && !raw.starts_with("/api/") {
                continue;
            }
            let Some(action) = data_source_action(raw) else {
                continue;
            };
            if !self.api_actions.contains_key(&action) {
                out.push(Local {
                    pos: full.start(),
                    severity: Severity::Error,
                    message: format!(
                        "data-source '{}' maps to API action '{}', which is not defined",
                        raw, action
                    ),
                });
            }
        }
    }
}

// ── scanning helpers ─────────────────────────────────────────────────

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn line_at(source: &str, position: usize) -> usize {
    source
        .as_bytes()
        .get(..position.min(source.len()))
        .map(|bytes| bytes.iter().filter(|&&byte| byte == b'\n').count() + 1)
        .unwrap_or(1)
}

fn line_of_needle(source: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 1;
    }
    source
        .find(needle)
        .map(|position| line_at(source, position))
        .unwrap_or(1)
}

fn collect_vlo(dir: &Path, skip_dir: Option<&str>, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if skip_dir
                .map(|skip| path.file_name().and_then(|name| name.to_str()) == Some(skip))
                .unwrap_or(false)
            {
                continue;
            }
            collect_vlo(&path, skip_dir, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("vlo") {
            files.push(path);
        }
    }
}

fn collect_component_stems(dir: &Path, names: &mut HashMap<String, Vec<String>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_component_stems(&path, names);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("vlo") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            names
                .entry(stem.to_ascii_lowercase())
                .or_default()
                .push(stem.to_string());
        }
    }
}

fn exact_vlo(dir: &Path, name: &str) -> Option<PathBuf> {
    let target = format!("{}.vlo", name);
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.file_name().and_then(|file| file.to_str()) == Some(target.as_str()))
}

fn data_source_action(raw: &str) -> Option<String> {
    let stripped = raw.strip_prefix("/api/").unwrap_or(raw);
    let path = stripped
        .split('?')
        .next()
        .unwrap_or(stripped)
        .trim_matches('/');
    if path.is_empty() || path.contains("{{") {
        return None;
    }
    let explicit = path.contains('/')
        || ["get_", "post_", "put_", "patch_", "delete_"]
            .iter()
            .any(|prefix| path.starts_with(prefix));
    Some(if explicit {
        path.to_string()
    } else {
        format!("get_{}", path)
    })
}

/// Regions whose contents must not be validated: code samples, client
/// scripts, HTML comments and `{raw}`/`{comment}` blocks.
fn masked_regions(source: &str) -> Vec<Region> {
    let lower = source.to_ascii_lowercase();
    let mut regions = Vec::new();

    for (open, close) in [
        ("<pre", "</pre>"),
        ("<code", "</code>"),
        ("<script", "</script>"),
        ("<textarea", "</textarea>"),
    ] {
        let mut from = 0usize;
        while let Some(offset) = lower[from..].find(open) {
            let start = from + offset;
            let after = start + open.len();
            let boundary = lower
                .as_bytes()
                .get(after)
                .map(|byte| byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/')
                .unwrap_or(true);
            if !boundary {
                from = after;
                continue;
            }
            let Some(close_offset) = lower[after..].find(close) else {
                break;
            };
            let end = after + close_offset + close.len();
            regions.push(Region { start, end });
            from = end;
        }
    }

    let mut from = 0usize;
    while let Some(offset) = source[from..].find("<!--") {
        let start = from + offset;
        let Some(close_offset) = source[start..].find("-->") else {
            break;
        };
        let end = start + close_offset + 3;
        regions.push(Region { start, end });
        from = end;
    }

    for (open, close) in [("{raw}", "{/raw}"), ("{comment}", "{/comment}")] {
        let mut from = 0usize;
        while let Some(offset) = source[from..].find(open) {
            let start = from + offset;
            let after = start + open.len();
            let Some(close_offset) = source[after..].find(close) else {
                break;
            };
            let end = after + close_offset + close.len();
            regions.push(Region { start, end });
            from = end;
        }
    }

    regions.sort_by_key(|region| region.start);
    regions
}

fn is_masked(regions: &[Region], position: usize) -> bool {
    regions
        .iter()
        .any(|region| position >= region.start && position < region.end)
}

fn find_opening_end(source: &str, from: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut quote = 0u8;
    let mut index = from;
    while index < bytes.len() {
        let byte = bytes[index];
        if quote != 0 {
            if byte == b'\\' {
                index += 1;
            } else if byte == quote {
                quote = 0;
            }
        } else if byte == b'"' || byte == b'\'' {
            quote = byte;
        } else if byte == b'>' {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Collects keys that sit at the top level of a JSON object, with their
/// byte offsets. serde_json silently keeps the last duplicate, so this
/// scan is the only way to report an action that was defined twice.
fn top_level_keys(source: &str) -> Vec<(String, usize)> {
    let bytes = source.as_bytes();
    let mut keys = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start = 0usize;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                let mut next = index + 1;
                while next < bytes.len() && (bytes[next] as char).is_whitespace() {
                    next += 1;
                }
                if depth == 1 && bytes.get(next) == Some(&b':') {
                    keys.push((source[string_start + 1..index].to_string(), string_start));
                }
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                string_start = index;
            }
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        index += 1;
    }

    keys
}

fn check_markers(source: &str, masked: &[Region], out: &mut Vec<Local>) {
    let mut stack: Vec<(&'static str, usize)> = Vec::new();
    let mut cursor = 0usize;

    while let Some((position, token)) = crate::template::find_next_token(source, cursor) {
        cursor = position + 1;
        if is_masked(masked, position) {
            continue;
        }

        if let Some((kind, _)) = stack.last() {
            if *kind == "raw" || *kind == "comment" {
                let closer = if *kind == "raw" { "{/raw}" } else { "{/comment}" };
                if token == closer {
                    stack.pop();
                }
                continue;
            }
        }

        match token {
            // `{{...}}` is an interpolation, not a block. Skip to its end so
            // the second brace is not mistaken for a `{/...}` closer, and
            // flag Handlebars-style directives, which VLO does not support.
            "{{" => {
                let end = source[position + 2..]
                    .find("}}")
                    .map(|offset| position + 2 + offset);
                let expression = match end {
                    Some(end) => &source[position + 2..end],
                    None => &source[position + 2..],
                };
                let expression = expression.trim();
                if expression.starts_with('#') {
                    let rendered = format!("{}{}{}", "{{", expression, "}}");
                    out.push(Local {
                        pos: position,
                        severity: Severity::Warning,
                        message: format!(
                            "'{}' is not VLO block syntax; use {{if ...}}/{{for ...}}",
                            rendered
                        ),
                    });
                }
                cursor = end.map(|end| end + 2).unwrap_or(position + 2);
            }
            "{if " => stack.push(("if", position)),
            "{for " => stack.push(("for", position)),
            "{unless " => stack.push(("unless", position)),
            "{switch " => stack.push(("switch", position)),
            "{with " => stack.push(("with", position)),
            "{capture " | "{capture}" => stack.push(("capture", position)),
            "{raw}" => stack.push(("raw", position)),
            "{comment}" => stack.push(("comment", position)),
            "{/if}" => close_marker(&mut stack, "if", position, out),
            "{/for}" => close_marker(&mut stack, "for", position, out),
            "{/unless}" => close_marker(&mut stack, "unless", position, out),
            "{/switch}" => close_marker(&mut stack, "switch", position, out),
            "{/with}" => close_marker(&mut stack, "with", position, out),
            "{/capture}" => close_marker(&mut stack, "capture", position, out),
            "{else}" => {
                if !stack
                    .iter()
                    .rev()
                    .any(|(kind, _)| matches!(*kind, "if" | "for" | "unless"))
                {
                    out.push(Local {
                        pos: position,
                        severity: Severity::Warning,
                        message: "{else} outside an {if} or {for} block".to_string(),
                    });
                }
            }
            "{else if " => {
                if !stack.iter().rev().any(|(kind, _)| *kind == "if") {
                    out.push(Local {
                        pos: position,
                        severity: Severity::Warning,
                        message: "{else if} outside an {if} block".to_string(),
                    });
                }
            }
            "{case " | "{default}" => {
                if !stack.iter().rev().any(|(kind, _)| *kind == "switch") {
                    out.push(Local {
                        pos: position,
                        severity: Severity::Warning,
                        message: format!("{} outside a {{switch}} block", token.trim_end_matches('}')),
                    });
                }
            }
            "{break}" | "{continue}" => {
                if !stack.iter().rev().any(|(kind, _)| *kind == "for") {
                    out.push(Local {
                        pos: position,
                        severity: Severity::Warning,
                        message: format!("{} outside a {{for}} loop", token.trim_end_matches('}')),
                    });
                }
            }
            _ => {}
        }
    }

    // Unclosed blocks are only reported when the file clearly closes that
    // kind elsewhere. Documentation pages contain one-line illustrations
    // such as `{if user}` that are meant to stay literal.
    let mut reported = HashSet::new();
    for (kind, position) in stack {
        if reported.insert(kind) && has_unmasked_closer(source, masked, kind) {
            out.push(Local {
                pos: position,
                severity: Severity::Warning,
                message: format!("unclosed {{{}}} block (no matching {{/{}}})", kind, kind),
            });
        }
    }
}

fn close_marker(
    stack: &mut Vec<(&'static str, usize)>,
    kind: &'static str,
    position: usize,
    out: &mut Vec<Local>,
) {
    match stack.iter().rposition(|(open, _)| *open == kind) {
        Some(index) => {
            while stack.len() > index + 1 {
                let (inner, inner_position) = stack.pop().unwrap();
                out.push(Local {
                    pos: inner_position,
                    severity: Severity::Warning,
                    message: format!("unclosed {{{}}} block", inner),
                });
            }
            stack.pop();
        }
        None => out.push(Local {
            pos: position,
            severity: Severity::Error,
            message: format!("unexpected {{/{}}} — no matching opening block", kind),
        }),
    }
}

fn has_unmasked_closer(source: &str, masked: &[Region], kind: &str) -> bool {
    let closer = format!("{{/{}}}", kind);
    let mut cursor = 0usize;
    while let Some((position, token)) = crate::template::find_next_token(source, cursor) {
        if token == "{{" {
            cursor = source[position + 2..]
                .find("}}")
                .map(|offset| position + 2 + offset + 2)
                .unwrap_or(position + 2);
            continue;
        }
        if token == closer && !is_masked(masked, position) {
            return true;
        }
        cursor = position + 1;
    }
    false
}

fn check_includes(root: &Path, source: &str, masked: &[Region], out: &mut Vec<Local>) {
    for captures in INCLUDE_RE.captures_iter(source) {
        let full = captures.get(0).unwrap();
        if is_masked(masked, full.start()) {
            continue;
        }
        let path = captures.get(1).unwrap().as_str();
        if path.is_empty() || path.contains("{{") {
            continue;
        }
        let candidates = [
            root.join(format!("{}.vlo", path)),
            root.join("components").join(format!("{}.vlo", path)),
            root.join("partials").join(format!("{}.vlo", path)),
        ];
        if !candidates.iter().any(|candidate| candidate.exists()) {
            out.push(Local {
                pos: full.start(),
                severity: Severity::Error,
                message: format!("{{include {}}} cannot be resolved", path),
            });
        }
    }
}
