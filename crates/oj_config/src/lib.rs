// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

mod schema;
pub use schema::*;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config parse error in {0}: {1}")]
    Parse(PathBuf, String),
    #[error("config evaluation error in {0}: {1}")]
    Eval(PathBuf, String),
    #[error("config schema error in {0}: {1}")]
    Schema(PathBuf, String),
}

const CANDIDATES: &[&str] = &[
    "oj.config.ts",
    "oj.config.mjs",
    "oj.config.js",
    "oj.config.json",
];

fn define_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn rolldown_options(config: &OjConfig) -> Option<&serde_json::Value> {
    let build = config.build.as_ref()?;
    build
        .rolldown_options
        .as_ref()
        .or(build.rollup_options.as_ref())
}

pub fn config_defines(config: &OjConfig) -> Vec<(String, String)> {
    config
        .define
        .as_ref()
        .map(|d| {
            d.iter()
                .map(|(k, v)| (k.clone(), define_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn environment_build_bool(config: &OjConfig, env_name: &str, field: &str) -> Option<bool> {
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("build"))
        .and_then(|b| b.get(field))
        .and_then(|v| v.as_bool())
}

pub fn resolve_conditions(config: &OjConfig, env_name: &str) -> Vec<String> {
    if let Some(c) = config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("conditions"))
        .and_then(|c| c.as_array())
    {
        return c
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    if let Some(c) = config.resolve.as_ref().and_then(|r| r.conditions.as_ref()) {
        return c.clone();
    }
    let base = if env_name == "ssr" { "node" } else { "browser" };
    [base, "import", "module", "default"]
        .map(String::from)
        .to_vec()
}

pub fn resolve_dedupe(config: &OjConfig) -> Vec<String> {
    config
        .resolve
        .as_ref()
        .and_then(|r| r.dedupe.as_ref())
        .cloned()
        .unwrap_or_default()
}

pub fn optimize_deps_lists(config: &OjConfig) -> (Vec<String>, Vec<String>, Vec<String>) {
    let od = config.optimize_deps.as_ref();
    let take = |f: Option<&Vec<String>>| f.cloned().unwrap_or_default();
    (
        take(od.and_then(|o| o.include.as_ref())),
        take(od.and_then(|o| o.exclude.as_ref())),
        take(od.and_then(|o| o.entries.as_ref())),
    )
}

pub fn resolve_alias(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    let mut merged: std::collections::BTreeMap<String, String> = config
        .resolve
        .as_ref()
        .and_then(|r| r.alias.as_ref())
        .map(|a| a.clone().into_iter().collect())
        .unwrap_or_default();
    if let Some(env_alias) = config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("alias"))
        .and_then(|a| a.as_object())
    {
        for (find, replacement) in env_alias {
            if let Some(s) = replacement.as_str() {
                merged.insert(find.clone(), s.to_string());
            }
        }
    }
    merged.into_iter().collect()
}

pub fn environment_defines(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    config
        .environments
        .as_ref()
        .and_then(|envs| envs.get(env_name))
        .and_then(|env| env.get("define"))
        .and_then(|d| d.as_object())
        .map(|d| {
            d.iter()
                .map(|(k, v)| (k.clone(), define_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn load(root: &Path) -> Result<OjConfig, ConfigError> {
    load_with(root, "serve", "development")
}

pub fn load_with(root: &Path, command: &str, mode: &str) -> Result<OjConfig, ConfigError> {
    let Some(path) = CANDIDATES
        .iter()
        .map(|c| root.join(c))
        .find(|p| p.is_file())
    else {
        return Ok(OjConfig::default());
    };
    let source = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?;

    let json = if path.extension().and_then(|e| e.to_str()) == Some("json") {
        source
    } else {
        evaluate(&path, &source, command, mode)?
    };
    serde_json::from_str(&json).map_err(|e| ConfigError::Schema(path, e.to_string()))
}

fn evaluate(path: &Path, source: &str, command: &str, mode: &str) -> Result<String, ConfigError> {
    let js = strip_types(path, source)?;
    let script = to_script(&js);

    let rt = rquickjs::Runtime::new()
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;
    let ctx = rquickjs::Context::full(&rt)
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;

    ctx.with(|ctx| {
        let env_obj: String = std::env::vars()
            .map(|(k, v)| {
                format!(
                    "{}:{}",
                    serde_json::to_string(&k).unwrap(),
                    serde_json::to_string(&v).unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let prelude = format!(
            "var defineConfig = function (x) {{ return x; }};\n\
             var process = {{ env: {{ {env_obj} }} }};\n\
             var globalThis = globalThis || this;\n"
        );
        let env_arg = format!(
            "{{ command: {}, mode: {}, isSsrBuild: false, isPreview: false }}",
            serde_json::to_string(command).unwrap(),
            serde_json::to_string(mode).unwrap()
        );
        let full = format!(
            "{prelude}{script}\n\
             var __ojC = globalThis.__ojConfig;\n\
             if (typeof __ojC === 'function') __ojC = __ojC({env_arg});\n\
             JSON.stringify(__ojC ?? null)"
        );
        let result: rquickjs::Value = ctx.eval(full).map_err(|e| {
            let caught = ctx.catch();
            let mut detail = caught
                .as_exception()
                .map(|ex| ex.to_string())
                .unwrap_or_else(|| format!("{e}"));
            if detail.contains("is not defined") {
                detail.push_str(
                    "\nnote: oj.config is evaluated in a sandbox without module imports; \
                     if this file is a plugins array, put it in oj.plugins.mjs instead",
                );
            }
            ConfigError::Eval(path.to_path_buf(), detail)
        })?;
        result
            .get::<String>()
            .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))
    })
}

fn strip_types(path: &Path, source: &str) -> Result<String, ConfigError> {
    use oxc_allocator::Allocator;
    use oxc_codegen::Codegen;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;
    use oxc_transformer::{TransformOptions, Transformer};

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            "syntax error".into(),
        ));
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();
    let ret = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            ret.diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    Ok(Codegen::new().build(&program).code)
}

fn to_script(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    for line in js.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("import{") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("export default ") {
            out.push_str("globalThis.__ojConfig = ");
            out.push_str(rest);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_config_in(label: &str, src: &str) -> OjConfig {
        let dir = std::env::temp_dir().join(format!("oj-cfg-{}-{label}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("oj.config.ts"), src).unwrap();
        let cfg = load(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        cfg
    }

    #[test]
    fn no_config_is_default() {
        let cfg = load(std::path::Path::new("/nonexistent-oj-root")).unwrap();
        assert!(cfg.server.is_none());
    }

    #[test]
    fn optimize_deps_and_dedupe_accessors() {
        let json = r#"{"resolve":{"dedupe":["react","react-dom"]},
            "optimizeDeps":{"include":["cjs-dep"],"exclude":["big-esm"],"entries":["src/main.tsx"]}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolve_dedupe(&cfg),
            vec!["react".to_string(), "react-dom".to_string()]
        );
        let (inc, exc, ent) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["cjs-dep".to_string()]);
        assert_eq!(exc, vec!["big-esm".to_string()]);
        assert_eq!(ent, vec!["src/main.tsx".to_string()]);
    }

    #[test]
    fn unknown_vite_keys_are_ignored_not_rejected() {
        // Vite never validates config-file keys; a config carrying options oj
        // doesn't model must load and keep its known fields, not hard-fail.
        let json = r#"{
            "base": "/app/",
            "resolve": { "dedupe": ["react"], "mainFields": ["module","browser"], "preserveSymlinks": true },
            "optimizeDeps": { "include": ["cjs-dep"], "esbuildOptions": { "target": "es2020" }, "needsInterop": ["x"] },
            "build": { "outDir": "out", "cssCodeSplit": false },
            "css": { "modules": {} },
            "worker": { "format": "es" },
            "logLevel": "silent"
        }"#;
        let cfg: OjConfig =
            serde_json::from_str(json).expect("unknown Vite keys must not fail config load");
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        assert_eq!(resolve_dedupe(&cfg), vec!["react".to_string()]);
        let (inc, _, _) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["cjs-dep".to_string()]);
    }

    #[test]
    fn optimize_deps_absent_is_empty() {
        let cfg: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(resolve_dedupe(&cfg).is_empty());
        let (inc, exc, ent) = optimize_deps_lists(&cfg);
        assert!(inc.is_empty() && exc.is_empty() && ent.is_empty());
    }

    #[test]
    fn evaluates_ts_config_with_types_and_define_config() {
        let cfg = eval_config_in(
            "define",
            "import { defineConfig } from \"oj\";\n\
             export default defineConfig({\n\
               server: { port: 3000, proxy: { \"/api\": \"http://localhost:8080\" } },\n\
               resolve: { alias: { \"@\": \"./src\" } as Record<string,string> },\n\
             });\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(3000));
        assert_eq!(
            server.proxy.unwrap().get("/api").unwrap().target(),
            "http://localhost:8080"
        );
        assert_eq!(
            cfg.resolve.unwrap().alias.unwrap().get("@").unwrap(),
            "./src"
        );
    }

    #[test]
    fn function_config_receives_command_and_mode() {
        let src = "export default ({ command, mode }) => ({ base: command === \"build\" ? \"/prod/\" : \"/dev/\", define: { __M__: mode } });\n";
        let cfg = eval_config_in("fnform", src);
        assert_eq!(cfg.base.as_deref(), Some("/dev/"));
        let defines: std::collections::BTreeMap<_, _> = config_defines(&cfg).into_iter().collect();
        assert_eq!(defines.get("__M__").unwrap(), "development");

        let dir = std::env::temp_dir().join(format!("oj-cfg-fnbuild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("oj.config.js"), src).unwrap();
        let cfg = load_with(&dir, "build", "production").unwrap();
        assert_eq!(cfg.base.as_deref(), Some("/prod/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undefined_reference_config_gives_plugins_hint() {
        let err = evaluate(
            std::path::Path::new("oj.config.mjs"),
            "export default [tailwindcss()];\n",
            "serve",
            "development",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tailwindcss"), "{msg}");
        assert!(msg.contains("oj.plugins.mjs"), "{msg}");
    }

    #[test]
    fn defineconfig_function_form_works() {
        let cfg = eval_config_in(
            "definefn",
            "import { defineConfig } from \"oj\";\nexport default defineConfig(({ mode }) => ({ bundle: mode === \"development\" }));\n",
        );
        assert_eq!(cfg.bundle, Some(true));
    }

    #[test]
    fn computed_values_and_process_env_work() {
        unsafe { std::env::set_var("OJ_TEST_PORT", "4321") };
        let cfg = eval_config_in(
            "computed",
            "export default { server: { port: Number(process.env.OJ_TEST_PORT), open: 1 > 0 } };\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(4321));
        assert_eq!(server.open, Some(true));
    }

    #[test]
    fn default_config_resolver_fallbacks() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cfg = load(std::path::Path::new("/nonexistent-oj-root")).unwrap();
        assert!(config_defines(&cfg).is_empty());
        assert!(environment_defines(&cfg, "ssr").is_empty());
        assert!(resolve_alias(&cfg, "client").is_empty());
        assert_eq!(environment_build_bool(&cfg, "client", "minify"), None);
        assert_eq!(
            resolve_conditions(&cfg, "ssr"),
            s(&["node", "import", "module", "default"])
        );
        assert_eq!(
            resolve_conditions(&cfg, "client"),
            s(&["browser", "import", "module", "default"])
        );
    }

    #[test]
    fn per_environment_resolution_and_precedence() {
        let cfg = eval_config_in(
            "env-resolvers",
            "export default {\n\
               define: { __FLAG__: \"true\", __COUNT__: 3 },\n\
               resolve: { conditions: [\"custom\"], alias: { \"@\": \"/src\", \"old\": \"/legacy\" } },\n\
               environments: {\n\
                 ssr: {\n\
                   build: { minify: false },\n\
                   resolve: { conditions: [\"node-only\"], alias: { \"old\": \"/ssr-legacy\" } },\n\
                   define: { __SSR__: true },\n\
                 },\n\
               },\n\
             };\n",
        );
        let defines: std::collections::BTreeMap<_, _> = config_defines(&cfg).into_iter().collect();
        assert_eq!(defines.get("__FLAG__").unwrap(), "true");
        assert_eq!(defines.get("__COUNT__").unwrap(), "3");

        assert_eq!(
            resolve_conditions(&cfg, "ssr"),
            vec!["node-only".to_string()]
        );
        assert_eq!(
            resolve_conditions(&cfg, "client"),
            vec!["custom".to_string()]
        );

        assert_eq!(
            resolve_alias(&cfg, "ssr"),
            vec![
                ("@".to_string(), "/src".to_string()),
                ("old".to_string(), "/ssr-legacy".to_string())
            ]
        );
        assert_eq!(
            resolve_alias(&cfg, "client"),
            vec![
                ("@".to_string(), "/src".to_string()),
                ("old".to_string(), "/legacy".to_string())
            ]
        );

        assert_eq!(environment_build_bool(&cfg, "ssr", "minify"), Some(false));
        assert_eq!(environment_build_bool(&cfg, "ssr", "sourcemap"), None);
        let ssr_defines: std::collections::BTreeMap<_, _> =
            environment_defines(&cfg, "ssr").into_iter().collect();
        assert_eq!(ssr_defines.get("__SSR__").unwrap(), "true");
        assert!(environment_defines(&cfg, "client").is_empty());
    }

    #[test]
    fn json_config_loads_directly() {
        let dir = std::env::temp_dir().join(format!("oj-cfg-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("oj.config.json"),
            r#"{"bundle":true,"base":"/app/"}"#,
        )
        .unwrap();
        let cfg = load(&dir).unwrap();
        assert_eq!(cfg.bundle, Some(true));
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
