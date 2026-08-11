// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Loads `oj.config.{ts,js,mjs,json}`.
//!
//! `.json` is parsed directly. For `.ts`/`.js`/`.mjs` we strip TypeScript
//! types with oxc, then evaluate the module in an embedded QuickJS engine
//! (no Node) with `defineConfig` and `process.env` shimmed, capturing the
//! default export and reading it back as JSON. Computed values, ternaries,
//! and `process.env` all work; the exported object must be JSON-serializable
//! (functions/plugins are out of scope until the plugin system).

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

const CANDIDATES: &[&str] = &["oj.config.ts", "oj.config.mjs", "oj.config.js", "oj.config.json"];

/// A `define` value as a JS-expression string. A JSON string is already the
/// expression the user wrote (e.g. `JSON.stringify("x")` -> `"x"`); anything
/// else is JSON-serialized (numbers/bools/objects are valid JS as-is).
fn define_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Top-level `config.define` as `(name, js-expression)` pairs, applied in every
/// environment.
pub fn config_defines(config: &OjConfig) -> Vec<(String, String)> {
    config
        .define
        .as_ref()
        .map(|d| d.iter().map(|(k, v)| (k.clone(), define_value(v))).collect())
        .unwrap_or_default()
}

/// A boolean under `environments.<name>.build.<field>` (e.g. `minify`,
/// `sourcemap`), if set — the per-environment build-output override.
pub fn environment_build_bool(config: &OjConfig, env_name: &str, field: &str) -> Option<bool> {
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("build"))
        .and_then(|b| b.get(field))
        .and_then(|v| v.as_bool())
}

/// Package `exports`/`imports` condition names for an environment. Precedence:
/// `environments.<name>.resolve.conditions` > top-level `resolve.conditions` >
/// the built-in default (browser for `client`, node for `ssr`).
pub fn resolve_conditions(config: &OjConfig, env_name: &str) -> Vec<String> {
    if let Some(c) = config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("conditions"))
        .and_then(|c| c.as_array())
    {
        return c.iter().filter_map(|v| v.as_str().map(String::from)).collect();
    }
    if let Some(c) = config.resolve.as_ref().and_then(|r| r.conditions.as_ref()) {
        return c.clone();
    }
    let base = if env_name == "ssr" { "node" } else { "browser" };
    [base, "import", "module", "default"].map(String::from).to_vec()
}

/// `resolve.alias` entries (`find`, `replacement`) for an environment.
/// Precedence: `environments.<name>.resolve.alias` merged over top-level
/// `resolve.alias`. Returns `(find, replacement)` pairs for the resolver.
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

/// `config.environments.<name>.define` as `(name, js-expression)` pairs — the
/// per-environment overrides of the Vite Environment API.
pub fn environment_defines(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    config
        .environments
        .as_ref()
        .and_then(|envs| envs.get(env_name))
        .and_then(|env| env.get("define"))
        .and_then(|d| d.as_object())
        .map(|d| d.iter().map(|(k, v)| (k.clone(), define_value(v))).collect())
        .unwrap_or_default()
}

/// Load the config from `root`, or `OjConfig::default()` if none exists.
pub fn load(root: &Path) -> Result<OjConfig, ConfigError> {
    let Some(path) = CANDIDATES.iter().map(|c| root.join(c)).find(|p| p.is_file()) else {
        return Ok(OjConfig::default());
    };
    let source = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?;

    let json = if path.extension().and_then(|e| e.to_str()) == Some("json") {
        source
    } else {
        evaluate(&path, &source)?
    };
    serde_json::from_str(&json).map_err(|e| ConfigError::Schema(path, e.to_string()))
}

/// Strip TS types, then evaluate as a script in QuickJS and return the
/// default-exported config as a JSON string.
fn evaluate(path: &Path, source: &str) -> Result<String, ConfigError> {
    let js = strip_types(path, source)?;
    let script = to_script(&js);

    let rt = rquickjs::Runtime::new()
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;
    let ctx = rquickjs::Context::full(&rt)
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;

    ctx.with(|ctx| {
        // Inject actual process.env so `process.env.X` in configs resolves.
        let env_obj: String = std::env::vars()
            .map(|(k, v)| format!("{}:{}", serde_json::to_string(&k).unwrap(), serde_json::to_string(&v).unwrap()))
            .collect::<Vec<_>>()
            .join(",");
        let prelude = format!(
            "var defineConfig = function (x) {{ return x; }};\n\
             var process = {{ env: {{ {env_obj} }} }};\n\
             var globalThis = globalThis || this;\n"
        );
        let full = format!("{prelude}{script}\nJSON.stringify(globalThis.__ojConfig ?? null)");
        let result: rquickjs::Value = ctx
            .eval(full)
            .map_err(|e| ConfigError::Eval(path.to_path_buf(), format!("{e}")))?;
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
        return Err(ConfigError::Parse(path.to_path_buf(), "syntax error".into()));
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
    let ret = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            ret.diagnostics.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("; "),
        ));
    }
    Ok(Codegen::new().build(&program).code)
}

/// Turn ESM `import ...; export default X` into a script that assigns the
/// config to `globalThis.__ojConfig`. Imports are stripped (only the shimmed
/// `defineConfig`/`process` are available); `export default` becomes a
/// capture assignment.
fn to_script(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    for line in js.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("import{") {
            continue; // drop imports; shims supply defineConfig
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
        // Unique dir per call so parallel tests never share a config file.
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
    fn evaluates_ts_config_with_types_and_define_config() {
        let cfg = eval_config_in("define",
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
        assert_eq!(cfg.resolve.unwrap().alias.unwrap().get("@").unwrap(), "./src");
    }

    #[test]
    fn computed_values_and_process_env_work() {
        unsafe { std::env::set_var("OJ_TEST_PORT", "4321") };
        let cfg = eval_config_in("computed",
            "export default { server: { port: Number(process.env.OJ_TEST_PORT), open: 1 > 0 } };\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(4321));
        assert_eq!(server.open, Some(true));
    }

    #[test]
    fn json_config_loads_directly() {
        let dir = std::env::temp_dir().join(format!("oj-cfg-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("oj.config.json"), r#"{"bundle":true,"base":"/app/"}"#).unwrap();
        let cfg = load(&dir).unwrap();
        assert_eq!(cfg.bundle, Some(true));
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
