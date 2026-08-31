// SPDX-License-Identifier: MIT
// StyleX serving: the per-module rule registry and sheet assembly.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fru::assemble::{AssembleConfig, LayersConfig};

pub use oj_compiler::stylex::{StylexPassConfig, StylexRule};

/// The dev-server side of the StyleX pass: the pass config, the cache salt,
/// and the per-module rule registry the served stylesheet is assembled from.
pub struct StylexState {
    pub pass: StylexPassConfig,
    /// blake3 of the config identity + pass version, folded into cache keys so
    /// a config or pass bump invalidates persisted modules.
    pub cache_salt: String,
    assemble_cfg: AssembleConfig,
    registry: Mutex<StylexRegistry>,
}

#[derive(Default)]
struct StylexRegistry {
    /// BTreeMap: file-path order pins a deterministic rule feed into assembly
    /// regardless of request order (matches the compiler's RuleRegistry).
    rules: BTreeMap<PathBuf, Vec<StylexRule>>,
    /// Served css URLs whose source contained the `@stylex;` directive; each
    /// gets a css-update repush when any stylex-gated module changes.
    css_urls: HashSet<String>,
    /// Watcher-marked modules, re-ensured lazily before the next assembly.
    dirty: HashSet<PathBuf>,
    generation: u64,
    assembled: Option<(u64, String)>,
}

fn assemble_config(pass: &StylexPassConfig) -> AssembleConfig {
    AssembleConfig {
        use_layers: if pass.use_css_layers {
            LayersConfig::On {
                before: Vec::new(),
                after: Vec::new(),
                prefix: None,
            }
        } else {
            LayersConfig::Off
        },
        ..AssembleConfig::default()
    }
}

/// One-shot assembly for the build path (the dev server memoizes via
/// `StylexState::assembled` instead).
pub fn assemble_sheet(rules: &[StylexRule], pass: &StylexPassConfig) -> Result<String, String> {
    fru::assemble::assemble(rules, &assemble_config(pass)).map_err(|e| e.to_string())
}

/// Shared `stylex` config resolution for dev and build: `--stylex-config` /
/// OJ_STYLEX_CONFIG override the config file's `stylex` section.
/// The zero-config default: enabled iff package.json depends on
/// @stylexjs/stylex, over the whole project except node_modules.
fn auto_config(root: &Path) -> Option<oj_config::StylexConfig> {
    let manifest = std::fs::read_to_string(root.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let has_dep = ["dependencies", "devDependencies"].iter().any(|key| {
        manifest
            .get(key)
            .and_then(|deps| deps.get("@stylexjs/stylex"))
            .is_some()
    });
    if !has_dep {
        return None;
    }
    println!("  stylex: auto-enabled (@stylexjs/stylex dependency detected)");
    Some(oj_config::StylexConfig {
        include: vec!["**".to_string()],
        exclude: vec!["node_modules/**".to_string()],
        ..oj_config::StylexConfig::default()
    })
}

pub fn resolve_pass_config(
    override_path: Option<&Path>,
    config: &oj_config::OjConfig,
    root: &Path,
    default_dev: bool,
) -> Result<Option<StylexPassConfig>, String> {
    let env_path = std::env::var("OJ_STYLEX_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let schema = match override_path.map(Path::to_path_buf).or(env_path) {
        Some(path) => {
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            Some(oj_config::load_stylex_json(&path).map_err(|e| e.to_string())?)
        }
        None => config.stylex.clone(),
    };
    let schema = match schema {
        Some(schema) => schema,
        // Auto pickup: a project that depends on @stylexjs/stylex gets the
        // pass with defaults; a `stylex` config section overrides everything.
        None => match auto_config(root) {
            Some(schema) => schema,
            None => return Ok(None),
        },
    };
    let root_dir = match &schema.root_dir {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                root.join(dir)
            }
        }
        None => root.to_path_buf(),
    };
    let pass = StylexPassConfig::new(
        root.to_path_buf(),
        root_dir,
        &schema.include,
        &schema.exclude,
        schema.dev.unwrap_or(default_dev),
        schema.use_css_layers.unwrap_or(false),
        schema.class_name_prefix.clone(),
    )?;
    Ok(Some(pass))
}

impl StylexState {
    pub fn new(pass: StylexPassConfig) -> Self {
        let cache_salt = blake3::hash(pass.salt_input().as_bytes())
            .to_hex()
            .to_string();
        let assemble_cfg = assemble_config(&pass);
        Self {
            pass,
            cache_salt,
            assemble_cfg,
            registry: Mutex::new(StylexRegistry::default()),
        }
    }

    /// Idempotent: bumps the generation (dropping the memoized sheet) only
    /// when this module's rules actually changed.
    pub fn register(&self, file: &Path, rules: &[StylexRule]) {
        let mut reg = self.registry.lock().unwrap();
        reg.dirty.remove(file);
        let changed = match reg.rules.get(file) {
            Some(prev) => prev.as_slice() != rules,
            None => !rules.is_empty(),
        };
        if !changed {
            return;
        }
        if rules.is_empty() {
            reg.rules.remove(file);
        } else {
            reg.rules.insert(file.to_path_buf(), rules.to_vec());
        }
        reg.generation += 1;
        reg.assembled = None;
    }

    pub fn remove(&self, file: &Path) {
        let mut reg = self.registry.lock().unwrap();
        reg.dirty.remove(file);
        if reg.rules.remove(file).is_some() {
            reg.generation += 1;
            reg.assembled = None;
        }
    }

    pub fn mark_dirty(&self, file: &Path) {
        self.registry
            .lock()
            .unwrap()
            .dirty
            .insert(file.to_path_buf());
    }

    pub fn take_dirty(&self) -> Vec<PathBuf> {
        self.registry.lock().unwrap().dirty.drain().collect()
    }

    pub fn record_css_url(&self, url: &str) {
        self.registry
            .lock()
            .unwrap()
            .css_urls
            .insert(url.to_string());
    }

    pub fn has_css_url(&self, url: &str) -> bool {
        self.registry.lock().unwrap().css_urls.contains(url)
    }

    pub fn css_urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = self
            .registry
            .lock()
            .unwrap()
            .css_urls
            .iter()
            .cloned()
            .collect();
        urls.sort();
        urls
    }

    pub fn generation(&self) -> u64 {
        self.registry.lock().unwrap().generation
    }

    /// The full assembled sheet, memoized per registry generation.
    pub fn assembled(&self) -> String {
        let mut reg = self.registry.lock().unwrap();
        if let Some((generation, css)) = &reg.assembled {
            if *generation == reg.generation {
                return css.clone();
            }
        }
        let all: Vec<StylexRule> = reg.rules.values().flatten().cloned().collect();
        // An assemble error (circular defineConsts reference) is an authoring
        // error; keep serving a sheet so the page stays usable.
        let css = match fru::assemble::assemble(&all, &self.assemble_cfg) {
            Ok(css) => css,
            Err(e) => {
                eprintln!("oj: stylex assemble error: {e}");
                format!("/* oj: stylex assemble error: {e} */")
            }
        };
        reg.assembled = Some((reg.generation, css.clone()));
        css
    }

    /// TEMPORARY smoke seam: OJ_STYLEX_FAKE_RULES (a JSON StylexRule array)
    /// seeds the registry at boot so serving is testable before the real pass.
    pub fn seed_fake_rules_from_env(&self, root: &Path) {
        let Ok(json) = std::env::var("OJ_STYLEX_FAKE_RULES") else {
            return;
        };
        match serde_json::from_str::<Vec<StylexRule>>(&json) {
            Ok(rules) => {
                println!(
                    "  stylex: seeded {} fake rule(s) from OJ_STYLEX_FAKE_RULES",
                    rules.len()
                );
                self.register(&root.join("__oj_stylex_fake__"), &rules);
            }
            Err(e) => eprintln!("oj: OJ_STYLEX_FAKE_RULES is not a StylexRule array: {e}"),
        }
    }
}

pub fn has_directive(source: &str) -> bool {
    source.contains("@stylex;")
}

/// Replace every `@stylex;` at-rule (the postcss-plugin convention) with the
/// assembled sheet; the caller checks `has_directive` first.
pub fn substitute_directive(source: &str, sheet: &str) -> String {
    source.replace("@stylex;", sheet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(class: &str, ltr: &str, priority: f64) -> StylexRule {
        StylexRule {
            class_name: class.into(),
            ltr: ltr.into(),
            rtl: None,
            const_key: None,
            const_val: None,
            priority,
        }
    }

    fn state_with(use_css_layers: bool) -> StylexState {
        StylexState::new(
            StylexPassConfig::new(
                PathBuf::from("/app"),
                PathBuf::from("/app"),
                &["src/**".into()],
                &[],
                true,
                use_css_layers,
                None,
            )
            .unwrap(),
        )
    }

    fn state() -> StylexState {
        state_with(false)
    }

    #[test]
    fn register_is_idempotent_and_generation_tracks_change() {
        let sx = state();
        let a = Path::new("/app/src/a.tsx");
        assert_eq!(sx.generation(), 0);
        sx.register(a, &[rule("x1", ".x1{color:red}", 3000.0)]);
        assert_eq!(sx.generation(), 1);
        sx.register(a, &[rule("x1", ".x1{color:red}", 3000.0)]);
        assert_eq!(sx.generation(), 1, "same rules must not bump");
        sx.register(a, &[rule("x2", ".x2{color:blue}", 3000.0)]);
        assert_eq!(sx.generation(), 2);
        sx.register(a, &[]);
        assert_eq!(sx.generation(), 3, "losing all rules is a change");
        sx.register(Path::new("/app/src/b.tsx"), &[]);
        assert_eq!(sx.generation(), 3, "empty on an unknown module is a no-op");
    }

    #[test]
    fn assembly_sorts_by_priority_dedupes_and_polyfills_specificity() {
        let sx = state();
        sx.register(
            Path::new("/app/src/a.tsx"),
            &[
                rule("x2", ".x2{margin:0}", 1000.0),
                rule("x1", ".x1{color:red}", 3000.0),
            ],
        );
        sx.register(
            Path::new("/app/src/b.tsx"),
            &[rule("x1", ".x1{color:red}", 3000.0)],
        );
        // Layers off: the second priority band gets the :not(#\#) polyfill.
        assert_eq!(sx.assembled(), ".x2{margin:0}\n.x1:not(#\\#){color:red}");
    }

    #[test]
    fn assembly_with_layers_emits_layer_header_and_blocks() {
        let sx = state_with(true);
        sx.register(
            Path::new("/app/src/a.tsx"),
            &[rule("x1", ".x1{color:red}", 3000.0)],
        );
        assert_eq!(
            sx.assembled(),
            "\n@layer priority1;\n@layer priority1{\n.x1{color:red}\n}"
        );
    }

    #[test]
    fn assembly_substitutes_define_consts() {
        let sx = state();
        let const_rule = StylexRule {
            class_name: "xconsthash".into(),
            ltr: String::new().into(),
            rtl: None,
            const_key: Some("--spacing".into()),
            const_val: Some(serde_json::json!("8px")),
            priority: 0.0,
        };
        sx.register(Path::new("/app/src/tokens.stylex.ts"), &[const_rule]);
        sx.register(
            Path::new("/app/src/a.tsx"),
            &[rule("x1", ".x1{padding:var(--xconsthash)}", 3000.0)],
        );
        assert_eq!(sx.assembled(), ".x1{padding:8px}");
    }

    #[test]
    fn assembly_is_memoized_per_generation() {
        let sx = state();
        sx.register(
            Path::new("/app/src/a.tsx"),
            &[rule("x1", ".x1{color:red}", 0.0)],
        );
        let first = sx.assembled();
        assert_eq!(sx.assembled(), first);
        sx.remove(Path::new("/app/src/a.tsx"));
        assert_eq!(sx.assembled(), "");
    }

    #[test]
    fn cache_salt_differs_across_configs() {
        let a = state().cache_salt;
        let b = StylexState::new(
            StylexPassConfig::new(
                PathBuf::from("/app"),
                PathBuf::from("/app"),
                &["app/**".into()],
                &[],
                true,
                false,
                None,
            )
            .unwrap(),
        )
        .cache_salt;
        let c = StylexState::new(
            StylexPassConfig::new(
                PathBuf::from("/app"),
                PathBuf::from("/app"),
                &["src/**".into()],
                &[],
                true,
                false,
                Some("oj".into()),
            )
            .unwrap(),
        )
        .cache_salt;
        assert_ne!(a, b);
        assert_ne!(a, c, "classNamePrefix must invalidate the cache");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn directive_substitution_replaces_the_at_rule_in_place() {
        let src = "@layer stylex { @stylex; }\nbody { margin: 0 }\n";
        assert!(has_directive(src));
        let out = substitute_directive(src, ".x1{color:red}\n");
        assert_eq!(
            out,
            "@layer stylex { .x1{color:red}\n }\nbody { margin: 0 }\n"
        );
        // "@stylexjs" in an import path is not the directive.
        assert!(!has_directive("@import \"@stylexjs/open-props\";"));
    }

    #[test]
    fn dirty_set_drains_and_forgets_removed_modules() {
        let sx = state();
        let a = PathBuf::from("/app/src/a.tsx");
        sx.mark_dirty(&a);
        assert_eq!(sx.take_dirty(), vec![a.clone()]);
        assert!(sx.take_dirty().is_empty());
        sx.mark_dirty(&a);
        sx.register(&a, &[rule("x1", ".x1{}", 0.0)]);
        assert!(sx.take_dirty().is_empty(), "register clears dirty");
    }
}
