// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use oj_server::plugins::PluginHost;
use rolldown::{
    BundlerBuilder, BundlerOptions, InputItem, OutputFormat, RawMinifyOptions, SourceMapType,
};
use rolldown_plugin::__inner::SharedPluginable;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookRenderChunkArgs, HookRenderChunkOutput,
    HookRenderChunkReturn, HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn,
    HookTransformArgs, HookTransformOutput, HookTransformOutputMap, HookTransformReturn, Plugin,
    PluginContext, SharedLoadPluginContext, SharedTransformPluginContext,
};

/// Both config loaders replace a function-valued rollup option with this marker
/// (the config is evaluated in a sandbox / a subprocess and travels as JSON, so
/// the function itself cannot reach the bundler). oj warns instead of silently
/// building without the option.
const FN_MARKER: &str = "__oj_fn__";

fn warn_fn_option(key: &str, hint: &str) {
    eprintln!(
        "oj build: (!) build.rollupOptions.{key} is a function, which oj cannot run (the \
         config is serialized before the build); the option is ignored. {hint}"
    );
}

fn is_fn_marker(v: &serde_json::Value) -> bool {
    v.as_str() == Some(FN_MARKER)
}

fn ro_output(ro: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    let output = ro?.get("output")?;
    if output.is_array() {
        output.get(0)
    } else {
        Some(output)
    }
}

fn ro_output_str(ro: Option<&serde_json::Value>, key: &str) -> Option<String> {
    let v = ro_output(ro)?.get(key)?;
    if is_fn_marker(v) {
        warn_fn_option(&format!("output.{key}"), "Use a pattern string such as \"assets/[name]-[hash].js\".");
        return None;
    }
    v.as_str().map(String::from)
}

fn ro_external(ro: Option<&serde_json::Value>) -> Vec<String> {
    match ro.and_then(|v| v.get("external")) {
        Some(serde_json::Value::String(s)) if s == FN_MARKER => {
            warn_fn_option("external", "List the external specifiers as strings.");
            Vec::new()
        }
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Debug)]
struct OjCssPlugin {
    collected: Arc<Mutex<Vec<(String, String)>>>,
    root: PathBuf,
    has_postcss: bool,
    client: bool,
    inline_limit: u64,
    css_code_split: bool,
    // The user plugin host, so CSS sources pass through the plugin `transform`
    // chain (e.g. UnoCSS's directive transformer resolving `@apply`) before oj
    // preprocesses/compiles them, matching Vite where CSS is a real module.
    host: Option<Arc<PluginHost>>,
    css_transform_enabled: Arc<tokio::sync::OnceCell<bool>>,
    /// `css.preprocessorOptions` (Sass additionalData/loadPaths, Less/Stylus
    /// options), applied the same way the dev server applies them.
    css: Option<oj_config::CssConfig>,
    /// `resolve.alias`, root and public dir for specifiers inside stylesheets.
    resolve: oj_css::CssResolveConfig,
    /// Inline `<script type="module">` bodies keyed by their html-proxy id.
    html_inline: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// What a `?worker&inline` bundle inherits from the app build (Vite's
    /// `bundleWorkerEntry` builds the worker under the app's resolved config).
    /// None: a bare browser bundle (server and library builds).
    worker: Option<Arc<WorkerBundleOpts>>,
}

/// The app build settings an inline worker bundle shares with its importer:
/// alias/conditions (`rolldown_resolve`), target and JSX (`transform_options`),
/// the client `define` map and minification.
#[derive(Debug)]
struct WorkerBundleOpts {
    config: oj_config::OjConfig,
    is_production: bool,
    define: Vec<(String, String)>,
    minify: bool,
}

/// What `@import`/`@use`/`url()` specifiers inside stylesheets resolve through,
/// as Vite's CSS resolvers do: the environment's `resolve.alias`, the root and
/// the public directory.
fn css_resolve_of(root: &Path, config: &oj_config::OjConfig, env: &str) -> oj_css::CssResolveConfig {
    oj_css::CssResolveConfig {
        root: root.to_path_buf(),
        public_dir: config
            .public_dir
            .as_ref()
            .map(|p| root.join(p))
            .unwrap_or_else(|| root.join("public")),
        alias: oj_config::resolve_alias(config, env),
    }
}

fn assets_inline_limit_of(config: &oj_config::OjConfig) -> u64 {
    config
        .build
        .as_ref()
        .and_then(|b| b.assets_inline_limit)
        .unwrap_or(4096)
}

/// Vite's `build.cssCodeSplit` defaults to true: each chunk gets its own CSS.
fn css_code_split_of(config: &oj_config::OjConfig) -> bool {
    config
        .build
        .as_ref()
        .and_then(|b| b.css_code_split)
        .unwrap_or(true)
}

fn re_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '/' => out.push_str(r"[\\/]"),
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Chunking from `output.advancedChunks` (rolldown's native form; wins when
/// present) or the Rollup `output.manualChunks` object. A manualChunks value is a
/// module id: a bare package name matches that package under `node_modules`, a
/// `./`-relative or absolute path matches that source file (Rollup semantics).
/// The function forms cannot cross the JSON boundary and produce a warning.
fn manual_chunks(
    ro: Option<&serde_json::Value>,
    root: &Path,
) -> Option<rolldown_common::CodeSplittingMode> {
    let output = ro_output(ro)?;
    if let Some(adv) = output.get("advancedChunks") {
        if let Some(mode) = advanced_chunks(adv) {
            return Some(mode);
        }
    }
    let mc = output.get("manualChunks")?;
    if is_fn_marker(mc) {
        warn_fn_option(
            "output.manualChunks",
            "Use the object form ({ vendor: [\"react\"] }) or rolldown's output.advancedChunks groups.",
        );
        return None;
    }
    let map = mc.as_object()?;
    let mut groups = Vec::new();
    let mut priority = map.len() as u32;
    for (name, tokens) in map {
        let Some(arr) = tokens.as_array() else {
            continue;
        };
        let mut packages: Vec<String> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        for t in arr.iter().filter_map(|t| t.as_str()) {
            if t.starts_with("./") || t.starts_with("../") || Path::new(t).is_absolute() {
                let abs = if Path::new(t).is_absolute() {
                    PathBuf::from(t)
                } else {
                    root.join(t)
                };
                let abs = abs.canonicalize().unwrap_or(abs);
                paths.push(re_escape(&abs.to_string_lossy()));
            } else {
                packages.push(re_escape(t));
            }
        }
        let mut alts: Vec<String> = Vec::new();
        if !packages.is_empty() {
            alts.push(format!(
                r"[\\/]node_modules[\\/]({})([\\/]|$)",
                packages.join("|")
            ));
        }
        if !paths.is_empty() {
            alts.push(format!("^({})$", paths.join("|")));
        }
        if alts.is_empty() {
            continue;
        }
        let pattern = alts.join("|");
        let Ok(test) = rolldown_utils::js_regex::HybridRegex::new(&pattern) else {
            continue;
        };
        groups.push(rolldown_common::MatchGroup {
            name: rolldown_common::MatchGroupName::Static(name.clone()),
            test: Some(rolldown_common::MatchGroupTest::Regex(test)),
            priority: Some(priority),
            ..Default::default()
        });
        priority = priority.saturating_sub(1);
    }
    if groups.is_empty() {
        return None;
    }
    Some(rolldown_common::CodeSplittingMode::Advanced(
        rolldown_common::ManualCodeSplittingOptions {
            groups: Some(groups),
            ..Default::default()
        },
    ))
}

/// rolldown `output.advancedChunks` from its JSON form. A group `test` is a
/// regex source (a RegExp literal arrives as `{ __oj_regex__ }`, a string is
/// taken as a regex like rolldown's own deserializer does); function tests and
/// names are warned about and skipped.
fn advanced_chunks(adv: &serde_json::Value) -> Option<rolldown_common::CodeSplittingMode> {
    let obj = adv.as_object()?;
    let f64_of = |k: &str| obj.get(k).and_then(|v| v.as_f64());
    let u32_of = |k: &str| obj.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
    let mut groups = Vec::new();
    for g in obj.get("groups").and_then(|g| g.as_array()).into_iter().flatten() {
        let Some(g) = g.as_object() else { continue };
        let name = match g.get("name") {
            Some(serde_json::Value::String(s)) if s == FN_MARKER => {
                warn_fn_option("output.advancedChunks.groups[].name", "Use a static name.");
                continue;
            }
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => continue,
        };
        let test = match g.get("test") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) if s == FN_MARKER => {
                warn_fn_option("output.advancedChunks.groups[].test", "Use a RegExp or string.");
                continue;
            }
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Object(o)) => o.get("__oj_regex__").and_then(|v| v.as_str()).map(str::to_string),
            _ => None,
        };
        let test = match test {
            Some(src) => match rolldown_utils::js_regex::HybridRegex::new(&src) {
                Ok(re) => Some(rolldown_common::MatchGroupTest::Regex(re)),
                Err(_) => {
                    eprintln!("oj build: (!) advancedChunks group {name}: invalid test regex {src:?}; skipped");
                    continue;
                }
            },
            None => None,
        };
        let gf = |k: &str| g.get(k).and_then(|v| v.as_f64());
        let gu = |k: &str| g.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
        groups.push(rolldown_common::MatchGroup {
            name: rolldown_common::MatchGroupName::Static(name),
            test,
            priority: gu("priority"),
            min_size: gf("minSize"),
            max_size: gf("maxSize"),
            min_share_count: gu("minShareCount"),
            min_module_size: gf("minModuleSize"),
            max_module_size: gf("maxModuleSize"),
            include_dependencies_recursively: g.get("includeDependenciesRecursively").and_then(|v| v.as_bool()),
            ..Default::default()
        });
    }
    if groups.is_empty() {
        return None;
    }
    Some(rolldown_common::CodeSplittingMode::Advanced(
        rolldown_common::ManualCodeSplittingOptions {
            groups: Some(groups),
            min_share_count: u32_of("minShareCount"),
            min_size: f64_of("minSize"),
            max_size: f64_of("maxSize"),
            min_module_size: f64_of("minModuleSize"),
            max_module_size: f64_of("maxModuleSize"),
            include_dependencies_recursively: obj.get("includeDependenciesRecursively").and_then(|v| v.as_bool()),
        },
    ))
}

/// rolldown's oxc transform settings: `build.target`, and the JSX runtime /
/// importSource / pragmas from `oxc.jsx` or `esbuild.jsx*` (what plugin-react's
/// `jsxImportSource` becomes). `development` follows NODE_ENV as in Vite, so a
/// development-mode build uses `jsx-dev-runtime`.
fn transform_options(
    config: &oj_config::OjConfig,
    is_production: bool,
) -> Option<rolldown_common::BundlerTransformOptions> {
    let jsx = oj_config::jsx_settings(config);
    let classic = jsx.runtime.as_deref() == Some("classic");
    Some(rolldown_common::BundlerTransformOptions {
        target: Some(rolldown_common::Either::Right(oj_config::build_targets(config))),
        // `runtime` stays None unless configured: rolldown only merges the
        // tsconfig `compilerOptions.jsx`/`jsxFactory`/`jsxImportSource` into the
        // transform when no runtime is set, and a project configured that way
        // must keep building as before.
        jsx: Some(rolldown_common::Either::Right(rolldown_common::JsxOptions {
            runtime: jsx.runtime.clone(),
            development: Some(!is_production),
            import_source: if classic { None } else { jsx.import_source },
            pragma: if classic { jsx.pragma } else { None },
            pragma_frag: if classic { jsx.pragma_frag } else { None },
            ..Default::default()
        })),
        ..Default::default()
    })
}

/// The three spellings Vite's define plugin replaces for NODE_ENV.
fn node_env_defines(node_env: &str) -> Vec<(String, String)> {
    let json = serde_json::Value::String(node_env.to_string()).to_string();
    [
        "process.env.NODE_ENV",
        "global.process.env.NODE_ENV",
        "globalThis.process.env.NODE_ENV",
    ]
    .iter()
    .map(|k| (k.to_string(), json.clone()))
    .collect()
}

/// Vite's define plugin for a browser bundle (`keepProcessEnv: false`, the
/// default for a client consumer and the SSR webworker target): `process.env`
/// itself (and its `global.`/`globalThis.` spellings) is defined to `{}`, so
/// `process.env.SOMETHING` evaluates to `undefined` instead of throwing a
/// ReferenceError, and NODE_ENV to the resolved mode. A lib build gets neither.
fn process_env_defines(node_env: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = ["process.env", "global.process.env", "globalThis.process.env"]
        .iter()
        .map(|k| (k.to_string(), "{}".to_string()))
        .collect();
    pairs.extend(node_env_defines(node_env));
    pairs
}

fn shell_node_env() -> Option<String> {
    std::env::var("NODE_ENV").ok().filter(|v| !v.is_empty())
}

/// `.env` files are read from `envDir` (default the root) and only `envPrefix`
/// variables (default `VITE_`) are exposed, as in dev; the build used to
/// hardcode both.
fn env_dir_of(root: &Path, config: &oj_config::OjConfig) -> PathBuf {
    match config.env_dir.as_deref() {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    }
}

fn env_prefixes_of(config: &oj_config::OjConfig) -> Vec<String> {
    oj_config::env_prefixes(config)
}

fn sourcemap_type(s: oj_config::Sourcemap) -> Option<SourceMapType> {
    match s {
        oj_config::Sourcemap::Off => None,
        oj_config::Sourcemap::File => Some(SourceMapType::File),
        oj_config::Sourcemap::Inline => Some(SourceMapType::Inline),
        oj_config::Sourcemap::Hidden => Some(SourceMapType::Hidden),
    }
}

/// `environments.<env>.build.sourcemap` (boolean) overrides the top-level setting.
fn env_sourcemap(
    config: &oj_config::OjConfig,
    env: &str,
    default: oj_config::Sourcemap,
) -> Option<SourceMapType> {
    match oj_config::environment_build_bool(config, env, "sourcemap") {
        Some(true) => Some(SourceMapType::File),
        Some(false) => None,
        None => sourcemap_type(default),
    }
}

/// Vite's `emptyOutDir` rule (watch.ts `resolveEmptyOutDir`, prepareOutDir.ts):
/// unset means "empty it when it is inside the project root"; an outDir outside
/// root is left alone with a warning unless `build.emptyOutDir` / `--emptyOutDir`
/// says so. A `.git` directory inside outDir always survives.
fn canonical_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Vite's `resolveEmptyOutDir`: an explicit setting wins; otherwise only an outDir
/// strictly inside root is emptied. An outDir equal to root is NOT inside (Vite
/// tests `startsWith(root + "/")`); treating it as inside emptied the whole
/// project, sources included. An outside outDir is left alone with a warning.
/// `warn` lets a second caller (the failed-build cleanup) reuse the decision
/// without repeating the message.
fn out_dir_emptiable(root: &Path, out_dir: &Path, empty: Option<bool>, warn: bool) -> bool {
    if let Some(b) = empty {
        return b;
    }
    let (r, o) = (canonical_or_self(root), canonical_or_self(out_dir));
    let inside = o != r && o.starts_with(&r);
    if !inside && warn && out_dir.exists() {
        eprintln!(
            "oj build: (!) outDir {} is not inside project root and will not be emptied.\n\
             Use --emptyOutDir to override.",
            out_dir.display()
        );
    }
    inside
}

/// Empty a directory the way Vite's `emptyDir(outDir, [".git"])` does: every entry
/// goes except a `.git` directory. Shared by the build-start emptying and the
/// failed-build cleanup, so both respect the same containment decision.
fn empty_dir_keep_git(out_dir: &Path) -> anyhow::Result<()> {
    if !out_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(out_dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(())
}

fn prepare_out_dir(root: &Path, out_dir: &Path, empty: Option<bool>) -> anyhow::Result<()> {
    if out_dir_emptiable(root, out_dir, empty, true) {
        // Emptying an outDir that IS the project root deletes the sources the
        // build reads, so it can never be what anyone meant, even with an explicit
        // emptyOutDir. Refuse rather than destroy the project (Vite would proceed).
        if canonical_or_self(out_dir) == canonical_or_self(root) {
            bail!(
                "build.outDir {} is the project root; refusing to empty it. Point outDir at a subdirectory.",
                out_dir.display()
            );
        }
        empty_dir_keep_git(out_dir)?;
    }
    fs::create_dir_all(out_dir)?;
    Ok(())
}

fn is_build_asset(id: &str) -> bool {
    oj_compiler::assets::is_asset_url(id)
}

fn asset_mime(ext: &str) -> &'static str {
    oj_compiler::assets::asset_mime(ext)
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn export_default_url(url: &str) -> String {
    format!(
        "export default {};",
        serde_json::Value::String(url.to_string())
    )
}

fn svg_data_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\\' => out.push_str("%5C"),
            '"' => out.push('\''),
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => {}
            other => out.push(other),
        }
    }
    format!("data:image/svg+xml,{out}")
}

fn emit_or_inline(
    ctx: &rolldown_plugin::SharedLoadPluginContext,
    file: &str,
    bytes: Vec<u8>,
    inline_limit: u64,
) -> anyhow::Result<String> {
    let path = std::path::Path::new(file);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if (bytes.len() as u64) <= inline_limit && ext != "svg" {
        return Ok(export_default_url(&format!(
            "data:{};base64,{}",
            asset_mime(ext),
            b64(&bytes)
        )));
    }
    if (bytes.len() as u64) <= inline_limit && ext == "svg" {
        let text = String::from_utf8_lossy(&bytes);
        return Ok(export_default_url(&svg_data_url(&text)));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("asset")
        .to_string();
    let reference = ctx
        .emit_file(
            rolldown_common::EmittedAsset {
                name: Some(name),
                source: rolldown_common::StrOrBytes::Bytes(bytes),
                ..Default::default()
            },
            None,
            None,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(format!(
        "export default import.meta.ROLLUP_FILE_URL_{reference};"
    ))
}

fn is_stylesheet_path(path: &str) -> bool {
    path.ends_with(".css")
        || oj_css::is_sass(path)
        || oj_server::sidecar::is_less(path)
        || oj_server::sidecar::is_stylus(path)
}

/// The build's stylesheet pipeline for one file: plugin transforms on the raw
/// source, Sass/Less/Stylus, Tailwind/PostCSS, then lightningcss (minified).
/// Shared by plain CSS imports, `?url` (emitted as a compiled `.css` asset) and
/// `?inline` (the compiled text), so every form of importing a stylesheet ships
/// the same CSS, as in Vite.
async fn compile_stylesheet(
    root: &Path,
    host: &Option<Arc<oj_server::plugins::PluginHost>>,
    css_transform_enabled: &tokio::sync::OnceCell<bool>,
    has_postcss: bool,
    css: &Option<oj_config::CssConfig>,
    resolve: &oj_css::CssResolveConfig,
    path: &str,
    id: &str,
) -> anyhow::Result<oj_css::CssOutput> {
    let cfg = oj_config::OjConfig {
        css: css.clone(),
        ..Default::default()
    };
    let is_less = oj_server::sidecar::is_less(path);
    let is_stylus = oj_server::sidecar::is_stylus(path);
    let mut source =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    // Run the plugin transform chain on the raw CSS source first, so directive
    // transformers (UnoCSS `@apply`/`@unocss-include`, etc.) resolve before oj
    // preprocesses and compiles it.
    if let Some(host) = host {
        let on = *css_transform_enabled
            .get_or_init(|| async { host.has_transform().await })
            .await;
        if on {
            if let Ok((out, _, _, _)) = host.transform(&source, id, "{}").await {
                source = out;
            }
        }
    }
    if oj_css::is_sass(path) {
        let lang = if path.ends_with(".sass") { "sass" } else { "scss" };
        let data = oj_config::css_additional_data(&cfg, lang);
        let load_paths: Vec<PathBuf> = oj_config::css_load_paths(&cfg, lang)
            .into_iter()
            .map(|p| root.join(p))
            .collect();
        let dir = std::path::Path::new(path).parent();
        source = oj_css::compile_sass_opts(
            &source,
            &oj_css::SassOptions {
                load_dir: dir,
                additional_data: data.as_deref(),
                load_paths: &load_paths,
                resolve: resolve.as_ref(),
            },
        )
        .map_err(|e| anyhow::anyhow!(e))?;
    } else if is_less || is_stylus {
        let lang = if is_less { "less" } else { "stylus" };
        if let Some(data) = oj_config::css_additional_data(&cfg, lang).filter(|d| !d.is_empty()) {
            source = format!("{data}\n{source}");
        }
        let opts = oj_config::css_preprocessor_json(&cfg, lang);
        source = preprocess_via_sidecar(root, std::path::Path::new(path), &source, opts)?;
    }
    // PostCSS/Tailwind run on the preprocessor OUTPUT (Vite orders them the same
    // way), on the source as transformed so far rather than re-read from disk.
    if oj_server::sidecar::is_tailwind_css(&source) || has_postcss {
        source = expand_css_via_sidecar(root, std::path::Path::new(path), &source)?;
    }
    // Plain `@import`s are inlined (postcss-import parity) so the concatenated
    // chunk stylesheet does not carry imports that 404 from `assets/`.
    source = oj_css::inline_imports_with(&source, std::path::Path::new(path), &resolve.as_ref())
        .map_err(|e| anyhow::anyhow!(e))?;
    let css_id = match std::path::Path::new(path).strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => path.to_string(),
    };
    oj_css::compile_css(&css_id, &source, true).map_err(|e| anyhow::anyhow!(e))
}

/// Split `spec?query` into the file and a normalized oj asset query, for Vite's
/// import queries in any combination/order: `?worker`, `?worker&inline`,
/// `?worker&url`, `?sharedworker...`, and the single `?url`/`?raw`/`?inline`/
/// `?init`/`?react`. Other queries (`?v=1`, `?tsr-split=x`) are not oj's.
fn split_asset_query(spec: &str) -> Option<(String, String)> {
    let (base, query) = spec.split_once('?')?;
    let params: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
    let has = |k: &str| params.iter().any(|p| *p == k);
    let worker_kind = if has("worker") {
        Some("worker")
    } else if has("sharedworker") {
        Some("sharedworker")
    } else {
        None
    };
    if let Some(kind) = worker_kind {
        let mut q = kind.to_string();
        if has("inline") {
            q.push_str("&inline");
        } else if has("url") {
            q.push_str("&url");
        }
        return Some((base.to_string(), q));
    }
    for kind in ["url", "init", "raw", "inline", "react"] {
        if params.len() == 1 && has(kind) {
            return Some((base.to_string(), kind.to_string()));
        }
    }
    None
}

/// `(file, ctor, inline, url)` for a resolved worker id (`x.ts?worker&inline`).
fn worker_id_parts(id: &str) -> Option<(&str, &'static str, bool, bool)> {
    let (file, query) = id.split_once('?')?;
    let params: Vec<&str> = query.split('&').collect();
    let ctor = if params.contains(&"worker") {
        "Worker"
    } else if params.contains(&"sharedworker") {
        "SharedWorker"
    } else {
        return None;
    };
    Some((file, ctor, params.contains(&"inline"), params.contains(&"url")))
}

/// Bundle a worker entry to a single ESM string (dynamic imports inlined) for
/// `?worker&inline`, so the worker ships inside the importing chunk as in Vite.
/// The worker is bundled the way Vite's `bundleWorkerEntry` does it: under the
/// app's resolve (alias, conditions), transform (target, JSX), define and minify
/// settings, with oj's own asset/CSS/worker plugin so `?url`, stylesheets and
/// nested workers inside the worker behave as in the main bundle. `nested` is
/// the plugin for that inner bundle, built by the caller from its own fields.
async fn bundle_worker_inline(
    root: &Path,
    file: &str,
    opts: Option<&Arc<WorkerBundleOpts>>,
    nested: OjCssPlugin,
) -> anyhow::Result<String> {
    let plugins: Vec<SharedPluginable> = vec![Arc::new(nested)];
    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("worker".to_string()),
                import: file.to_string(),
                ..Default::default()
            }]),
            cwd: Some(root.to_path_buf()),
            format: Some(OutputFormat::Esm),
            platform: Some(rolldown::Platform::Browser),
            code_splitting: Some(rolldown_common::CodeSplittingMode::Bool(false)),
            resolve: opts.and_then(|o| rolldown_resolve(root, &o.config, "client")),
            transform: opts.and_then(|o| transform_options(&o.config, o.is_production)),
            define: opts.map(|o| o.define.iter().cloned().collect()),
            minify: Some(RawMinifyOptions::Bool(opts.is_none_or(|o| o.minify))),
            ..Default::default()
        })
        .build()
        .map_err(|errs| anyhow::anyhow!("inline worker init failed: {errs:?}"))?;
    let out = bundler
        .generate()
        .await
        .map_err(|errs| anyhow::anyhow!("inline worker build failed for {file}:\n{errs:?}"))?;
    let mut code = String::new();
    for asset in &out.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            code.push_str(&chunk.code);
        }
    }
    Ok(code)
}

impl Plugin for OjCssPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:build")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
            | rolldown_plugin::HookUsage::Load
            | rolldown_plugin::HookUsage::Transform
    }

    fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let is_routes = args.specifier == "virtual:oj-routes";
        let routes_id = self
            .root
            .join("oj-routes.tsx")
            .to_string_lossy()
            .into_owned();
        let asset_query = split_asset_query(args.specifier);
        let importer = args.importer.map(str::to_string);
        let html_proxy = args.specifier.contains("?html-proxy&index=").then(|| {
            let spec = args.specifier.strip_prefix("./").unwrap_or(args.specifier);
            if Path::new(spec).is_absolute() {
                spec.to_string()
            } else {
                self.root.join(spec).to_string_lossy().into_owned()
            }
        });
        let ctx = ctx.clone();
        async move {
            if is_routes {
                return Ok(Some(HookResolveIdOutput::from_id(routes_id)));
            }
            if let Some(id) = html_proxy {
                return Ok(Some(HookResolveIdOutput::from_id(id)));
            }
            if let Some((base, query)) = asset_query {
                if let Ok(Ok(resolved)) = ctx.resolve(&base, importer.as_deref(), None).await {
                    let id = format!("{}?{query}", resolved.id.as_str());
                    return Ok(Some(HookResolveIdOutput::from_id(id)));
                }
            }
            Ok(None)
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let id = args.id.to_string();
        let code = args.code.to_string();
        async move {
            let has_glob = code.contains("import.meta.glob");
            let has_dynamic = code.contains("import(");
            let has_new_url = code.contains("import.meta.url");
            if !has_glob && !has_dynamic && !has_new_url {
                return Ok(None);
            }
            let path = std::path::Path::new(&id);
            let mut expanded = code;
            if has_glob {
                expanded = oj_compiler::glob::expand_source(&expanded, path);
            }
            if has_dynamic {
                expanded = oj_compiler::glob::expand_dynamic_import_vars_source(&expanded, path);
            }
            if has_new_url {
                expanded = oj_compiler::glob::expand_new_url_asset_source(&expanded, path);
            }
            Ok(Some(rolldown_plugin::HookTransformOutput {
                code: Some(expanded),
                ..Default::default()
            }))
        }
    }

    fn load(
        &self,
        ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let id = args.id.to_string();
        let collected = Arc::clone(&self.collected);
        let root = self.root.clone();
        let routes_id = root.join("oj-routes.tsx").to_string_lossy().into_owned();
        let client = self.client;
        let inline_limit = self.inline_limit;
        let host = self.host.clone();
        let css_transform_enabled = Arc::clone(&self.css_transform_enabled);
        let self_has_postcss = self.has_postcss;
        let css_cfg = self.css.clone();
        let css_resolve = self.resolve.clone();
        let html_inline = Arc::clone(&self.html_inline);
        let worker = self.worker.clone();
        async move {
            if id.contains("?html-proxy&index=") {
                if let Some(body) = html_inline.lock().unwrap().get(&id).cloned() {
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(body),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
            }
            if let Some(file) = id.strip_suffix("?url") {
                if is_stylesheet_path(file) {
                    // `import u from './a.scss?url'`: the URL of the COMPILED
                    // stylesheet, emitted as a `.css` asset (Vite css.ts), not the
                    // raw preprocessor source.
                    let out = compile_stylesheet(
                        &root,
                        &host,
                        &css_transform_enabled,
                        self_has_postcss,
                        &css_cfg,
                        &css_resolve,
                        file,
                        &id,
                    )
                    .await?;
                    let stem = std::path::Path::new(file)
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("style");
                    let reference = ctx
                        .emit_file(
                            rolldown_common::EmittedAsset {
                                name: Some(format!("{stem}.css")),
                                source: rolldown_common::StrOrBytes::Str(out.css),
                                ..Default::default()
                            },
                            None,
                            None,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(format!(
                            "export default import.meta.ROLLUP_FILE_URL_{reference};"
                        )),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let code = emit_or_inline(&ctx, file, bytes, inline_limit)?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            // A plain `.svg` (no query) goes through the plugin transform chain
            // (vite-plugin-svgr) so it becomes a React component in the prod build
            // too, matching dev and Vite. Run the transform on the raw svg; if a
            // plugin componentizes it, emit JSX. An svg no plugin matches leaves the
            // markup unchanged and falls through to the asset pipeline below. The
            // transform hook skips `.svg` (it already ran here) to avoid running it
            // twice on the componentized output.
            if !id.contains('?') && id.ends_with(".svg") {
                if let Some(host) = &host {
                    let svg = std::fs::read_to_string(&id)
                        .map_err(|e| anyhow::anyhow!("cannot read {id}: {e}"))?;
                    if let Ok((out, _, _, _)) = host.transform(&svg, &id, "{}").await {
                        if out != svg && !out.trim_start().starts_with('<') {
                            return Ok(Some(rolldown_plugin::HookLoadOutput {
                                code: arcstr::ArcStr::from(out),
                                module_type: Some(rolldown_common::ModuleType::Jsx),
                                ..Default::default()
                            }));
                        }
                    }
                }
            }
            if !id.contains('?') && is_build_asset(&id) {
                let bytes =
                    std::fs::read(&id).map_err(|e| anyhow::anyhow!("cannot read {id}: {e}"))?;
                let code = emit_or_inline(&ctx, &id, bytes, inline_limit)?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?init") {
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let name = std::path::Path::new(file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("asset")
                    .to_string();
                let reference = ctx
                    .emit_file(
                        rolldown_common::EmittedAsset {
                            name: Some(name),
                            source: rolldown_common::StrOrBytes::Bytes(bytes),
                            ..Default::default()
                        },
                        None,
                        None,
                    )
                    .map_err(|e| anyhow::anyhow!(e))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "const u = import.meta.ROLLUP_FILE_URL_{reference};\nexport default (imports = {{}}) => {{ const inst = (r) => r.instance; const fb = () => fetch(u).then((r) => r.arrayBuffer()).then((b) => WebAssembly.instantiate(b, imports)).then(inst); return WebAssembly.instantiateStreaming ? WebAssembly.instantiateStreaming(fetch(u), imports).then(inst).catch(fb) : fb(); }};"
                    )),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?raw") {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "export default {};",
                        serde_json::Value::String(text)
                    )),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?inline") {
                if is_stylesheet_path(file) {
                    // `import css from './a.css?inline'` is the compiled CSS text,
                    // as in dev and Vite, not a base64 data URI of the source.
                    let out = compile_stylesheet(
                        &root,
                        &host,
                        &css_transform_enabled,
                        self_has_postcss,
                        &css_cfg,
                        &css_resolve,
                        file,
                        &id,
                    )
                    .await?;
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(format!(
                            "export default {};",
                            serde_json::Value::String(out.css)
                        )),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let ext = std::path::Path::new(file)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "export default \"data:{};base64,{}\";",
                        asset_mime(ext),
                        b64(&bytes)
                    )),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?react") {
                let svg = std::fs::read_to_string(file)
                    .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(oj_server::svgr::svg_to_component(&svg)),
                    module_type: Some(rolldown_common::ModuleType::Jsx),
                    ..Default::default()
                }));
            }
            if oj_server::sidecar::is_svelte(&id) {
                let js = svelte_via_sidecar(&root, std::path::Path::new(&id))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(js),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some((file, ctor, inline, url)) = worker_id_parts(&id) {
                if inline {
                    // `?worker&inline`: the worker's bundled code travels inside
                    // this chunk and starts from a Blob URL (data: URL fallback).
                    let nested = OjCssPlugin {
                        collected: Arc::new(Mutex::new(Vec::new())),
                        root: root.clone(),
                        has_postcss: self_has_postcss,
                        client: true,
                        inline_limit,
                        css_code_split: false,
                        host: host.clone(),
                        css_transform_enabled: Arc::clone(&css_transform_enabled),
                        css: css_cfg.clone(),
                        html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
                        worker: worker.clone(),
                    };
                    let code = bundle_worker_inline(&root, file, worker.as_ref(), nested).await?;
                    let literal = serde_json::Value::String(code).to_string();
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(format!(
                            "const __oj_worker_code = {literal};\nexport default function (options) {{ const opts = Object.assign({{ type: \"module\" }}, options); const blob = typeof Blob !== \"undefined\" && new Blob([__oj_worker_code], {{ type: \"text/javascript;charset=utf-8\" }}); const url = blob ? URL.createObjectURL(blob) : \"data:text/javascript;charset=utf-8,\" + encodeURIComponent(__oj_worker_code); try {{ return new {ctor}(url, opts); }} finally {{ if (blob) setTimeout(() => URL.revokeObjectURL(url), 0); }} }};"
                        )),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
                let stem = std::path::Path::new(file)
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("worker")
                    .to_string();
                let reference = ctx
                    .emit_chunk(rolldown_common::EmittedChunk {
                        id: file.to_string(),
                        name: Some(stem.into()),
                        preserve_entry_signatures: Some(
                            rolldown_common::PreserveEntrySignatures::False,
                        ),
                        ..Default::default()
                    })
                    .map_err(|e| anyhow::anyhow!(e))?;
                let code = if url {
                    // `?worker&url` (also what `new Worker(new URL(...))` becomes):
                    // the chunk's URL as a string.
                    format!("export default import.meta.ROLLUP_FILE_URL_{reference};")
                } else {
                    format!(
                        "export default function (options) {{ return new {ctor}(import.meta.ROLLUP_FILE_URL_{reference}, Object.assign({{ type: \"module\" }}, options)); }};"
                    )
                };
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            let path = id.split('?').next().unwrap_or(&id);
            if client && is_server_module_path(path) {
                let source = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
                let url = match std::path::Path::new(path).strip_prefix(&root) {
                    Ok(rel) => format!("/{}", rel.display()),
                    Err(_) => path.to_string(),
                };
                let stub = server_fn_prod_stub(
                    &oj_compiler::exports(&source, std::path::Path::new(path)),
                    &url,
                );
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(stub),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if path == routes_id {
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(oj_server::OJ_ROUTES_JS),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if !is_stylesheet_path(path) {
                return Ok(None);
            }
            let output = compile_stylesheet(
                &root,
                &host,
                &css_transform_enabled,
                self_has_postcss,
                &css_cfg,
                &css_resolve,
                path,
                &id,
            )
            .await?;
            // A CSS module: the class map as default plus named exports per
            // identifier-safe class (Vite's dataToEsm with namedExports).
            let js = match &output.exports {
                Some(exports) => oj_css::css_modules_esm(exports),
                None => "export default void 0;".to_string(),
            };
            collected
                .lock()
                .unwrap()
                .push((path.to_string(), output.css));
            // When code-splitting CSS, keep the stub in its chunk so `module_ids`
            // maps each stylesheet to the chunk that imported it; without this the
            // unused-default stub is tree-shaken and the mapping is lost.
            let side_effects = self
                .css_code_split
                .then_some(rolldown_common::side_effects::HookSideEffects::NoTreeshake);
            Ok(Some(rolldown_plugin::HookLoadOutput {
                code: arcstr::ArcStr::from(js),
                module_type: Some(rolldown_common::ModuleType::Js),
                side_effects,
                ..Default::default()
            }))
        }
    }
}

/// Tailwind / PostCSS over `css` (the file's content as processed so far, not
/// re-read from disk), via the sidecar's line protocol.
fn expand_css_via_sidecar(root: &Path, css_file: &Path, css: &str) -> anyhow::Result<String> {
    run_sidecar_once(
        root,
        "css-sidecar.mjs",
        oj_server::sidecar::SIDECAR_JS,
        css_file,
        css,
        serde_json::Value::Null,
        "tailwind/postcss",
    )
}

fn run_sidecar_once(
    root: &Path,
    script_name: &str,
    script_src: &str,
    css_file: &Path,
    css: &str,
    options: serde_json::Value,
    what: &str,
) -> anyhow::Result<String> {
    use std::io::Write;
    let script = oj_cache::cache_root(root).join(script_name);
    if let Some(parent) = script.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&script, script_src)?;
    let req = serde_json::json!({
        "id": 1,
        "base": root.to_string_lossy(),
        "css": css,
        "from": css_file.to_string_lossy(),
        "options": options,
    })
    .to_string();
    let postcss_config = oj_server::find_postcss_config(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut child = std::process::Command::new("node")
        .arg(&script)
        .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(root))
        .env("OJ_POSTCSS_CONFIG", postcss_config)
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("node not found for {what} build"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{req}\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim().lines().next().unwrap_or("{}");
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    match v.get("css").and_then(|c| c.as_str()) {
        Some(css) => Ok(css.to_string()),
        None => bail!(
            "{what} failed for {}: {}",
            css_file.display(),
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("sidecar produced no output")
        ),
    }
}

fn svelte_via_sidecar(root: &Path, file: &Path) -> anyhow::Result<String> {
    use std::io::Write;
    let script = oj_cache::cache_root(&root).join("svelte-compile.mjs");
    if let Some(parent) = script.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&script, oj_server::sidecar::SVELTE_COMPILE_JS)?;
    let source = fs::read_to_string(file)?;
    let req = serde_json::json!({
        "id": 1,
        "base": root.to_string_lossy(),
        "css": source,
        "from": file.to_string_lossy(),
        "dev": false,
    })
    .to_string();
    let mut child = std::process::Command::new("node")
        .arg(&script)
        .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(root))
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("node not found for svelte compile")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{req}\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim().lines().next().unwrap_or("{}");
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
    match v.get("css").and_then(|c| c.as_str()) {
        Some(js) => Ok(js.to_string()),
        None => bail!(
            "svelte compile failed for {}: {}",
            file.display(),
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("is `svelte` installed?")
        ),
    }
}

fn preprocess_via_sidecar(
    root: &Path,
    css_file: &Path,
    css: &str,
    options: serde_json::Value,
) -> anyhow::Result<String> {
    run_sidecar_once(
        root,
        "css-preprocess.mjs",
        oj_server::sidecar::PREPROCESS_JS,
        css_file,
        css,
        options,
        "css preprocess",
    )
    .map_err(|e| anyhow::anyhow!("{e} (is `less`/`stylus` installed?)"))
}

fn rolldown_resolve(
    root: &Path,
    config: &oj_config::OjConfig,
    env: &str,
) -> Option<rolldown_common::ResolveOptions> {
    let alias = oj_config::resolve_alias(config, env);
    let alias: Vec<(String, Vec<Option<String>>)> = alias
        .into_iter()
        .map(|(find, replacement)| {
            let target = if replacement.starts_with('.') {
                root.join(&replacement).to_string_lossy().into_owned()
            } else {
                replacement
            };
            (find, vec![Some(target)])
        })
        .collect();
    // The same `resolve.*` the dev resolver honors, so a module that resolves in
    // dev (a custom extension, a mainFields order, an exports condition) also
    // resolves in the build. Only user-set values are forwarded; rolldown keeps
    // its own defaults for the rest.
    let extensions = oj_config::resolve_extensions(config);
    let main_fields = oj_config::resolve_main_fields(config);
    // Conditions are always explicit: Vite resolves a build with `production`
    // (its `development|production` default), and the dev server passes the
    // same list, so a dep with a `development` export never differs between
    // dev and build.
    let condition_names = Some(oj_config::resolve_conditions_for(config, env, false));
    let symlinks = config
        .resolve
        .as_ref()
        .and_then(|r| r.preserve_symlinks)
        .map(|preserve| !preserve);
    Some(rolldown_common::ResolveOptions {
        alias: (!alias.is_empty()).then_some(alias),
        extensions,
        main_fields,
        condition_names,
        symlinks,
        // `.js` -> `.ts` remap for every fs path (aliases included), as in dev.
        extension_alias: Some(oj_resolver::default_extension_alias()),
        ..Default::default()
    })
}

fn is_server_module_path(path: &str) -> bool {
    [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
        .iter()
        .any(|s| path.ends_with(s))
}

fn server_fn_prod_stub(exports: &[String], url: &str) -> String {
    let mut out = String::from(
        "const __ojCall = (m, n, a) => fetch(\"/__oj_fn\", { method: \"POST\", \
         headers: { \"content-type\": \"application/json\" }, \
         body: JSON.stringify({ module: m, name: n, args: a }) })\
         .then((r) => { if (!r.ok) throw new Error(\"oj server fn \" + n + \": \" + r.status); return r.json(); });\n",
    );
    for name in exports {
        if name == "default" {
            out.push_str(&format!(
                "export default (...a) => __ojCall({url:?}, \"default\", a);\n"
            ));
        } else {
            out.push_str(&format!(
                "export const {name} = (...a) => __ojCall({url:?}, {name:?}, a);\n"
            ));
        }
    }
    out
}

fn copy_public_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            fs::create_dir_all(&to)?;
            copy_public_dir(&from, &to)?;
        } else if !to.exists() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// Shared state for chunks/pages a plugin emits during the build. Held by both
// OjUserPlugin (which fills it inside hooks) and build() (which reads the
// collected HTML pages afterward to render them).
#[derive(Debug)]
struct EmitState {
    root: PathBuf,
    // oj chunk ref id -> rolldown ref id, resolved to the hashed name later.
    chunk_refs: std::sync::Mutex<std::collections::HashMap<String, String>>,
    // oj ref id -> an already-resolved output name (emitted `.html` pages, whose
    // output path is known at emit time rather than via rolldown).
    direct_names: std::sync::Mutex<std::collections::HashMap<String, String>>,
    // `.html` pages a plugin emitted as chunks, to render after the build.
    html_docs: std::sync::Mutex<Vec<HtmlDoc>>,
}

impl EmitState {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            chunk_refs: std::sync::Mutex::new(std::collections::HashMap::new()),
            direct_names: std::sync::Mutex::new(std::collections::HashMap::new()),
            html_docs: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[derive(Debug)]
struct OjUserPlugin {
    host: Arc<PluginHost>,
    render_chunk_enabled: Arc<tokio::sync::OnceCell<bool>>,
    emit: Arc<EmitState>,
}

impl OjUserPlugin {
    fn new(host: Arc<PluginHost>, emit: Arc<EmitState>) -> Self {
        Self {
            host,
            render_chunk_enabled: Arc::new(tokio::sync::OnceCell::new()),
            emit,
        }
    }
}

// The rolldown module type a plugin-loaded id should parse as, from its
// extension (query stripped). Defaults to Js, matching Rollup/Vite treating an
// extensionless virtual id as JavaScript.
fn module_type_for_id(id: &str) -> rolldown_common::ModuleType {
    use rolldown_common::ModuleType;
    let path = id.split(['?', '#']).next().unwrap_or(id);
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("jsx") => ModuleType::Jsx,
        Some("tsx") => ModuleType::Tsx,
        Some("ts" | "mts" | "cts") => ModuleType::Ts,
        Some("json") => ModuleType::Json,
        _ => ModuleType::Js,
    }
}

// Forward chunks a plugin emitted (in buildStart or transform) to rolldown as
// build roots. A `.html` id is not a JS module: its `<script type=module>` are
// emitted as JS entries instead and the page is queued for rendering, with the
// ref id resolving to the page's output path.
fn forward_emitted_chunks(
    ctx: &PluginContext,
    chunks: &[oj_server::plugins::ChunkEmit],
    emit: &EmitState,
) {
    if chunks.is_empty() {
        return;
    }
    for c in chunks {
        if c.id.ends_with(".html") {
            forward_emitted_html(ctx, c, emit);
            continue;
        }
        let emitted = rolldown_common::EmittedChunk {
            id: c.id.clone(),
            name: c.name.clone().map(Into::into),
            file_name: c.file_name.clone().map(Into::into),
            ..Default::default()
        };
        if let Ok(rd_ref) = ctx.emit_chunk(emitted) {
            emit.chunk_refs
                .lock()
                .unwrap()
                .insert(c.ref_id.clone(), rd_ref.to_string());
        }
    }
}

fn forward_emitted_html(
    ctx: &PluginContext,
    c: &oj_server::plugins::ChunkEmit,
    emit: &EmitState,
) {
    let abs = if Path::new(&c.id).is_absolute() {
        PathBuf::from(&c.id)
    } else {
        emit.root.join(&c.id)
    };
    let Ok(content) = fs::read_to_string(&abs) else {
        return;
    };
    let html_dir = abs.parent().unwrap_or(&emit.root).to_path_buf();
    let out_rel = abs
        .strip_prefix(&emit.root)
        .map(|p| p.display().to_string().replace('\\', "/"))
        .unwrap_or_else(|_| {
            abs.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.id.clone())
        });
    let scripts: Vec<HtmlScript> = module_script_srcs(&content)
        .into_iter()
        .map(|src| {
            let sabs = resolve_html_ref(&src, &html_dir, &emit.root);
            let emitted = rolldown_common::EmittedChunk {
                id: sabs.display().to_string(),
                name: sabs
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(Into::into),
                ..Default::default()
            };
            let _ = ctx.emit_chunk(emitted);
            HtmlScript { src, abs: sabs }
        })
        .collect();
    emit.direct_names
        .lock()
        .unwrap()
        .insert(c.ref_id.clone(), out_rel.clone());
    emit.html_docs.lock().unwrap().push(HtmlDoc {
        out_rel,
        src_html: content,
        scripts,
        dir: emit.root.clone(),
    });
}

impl Plugin for OjUserPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:user-plugins")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::BuildStart
            | rolldown_plugin::HookUsage::ResolveId
            | rolldown_plugin::HookUsage::Load
            | rolldown_plugin::HookUsage::Transform
            | rolldown_plugin::HookUsage::GenerateBundle
            | rolldown_plugin::HookUsage::RenderChunk
            | rolldown_plugin::HookUsage::WriteBundle
            | rolldown_plugin::HookUsage::RenderStart
            | rolldown_plugin::HookUsage::CloseBundle
    }

    async fn build_start(
        &self,
        ctx: &PluginContext,
        _args: &rolldown_plugin::HookBuildStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        // Vite/rolldown: a rejecting buildStart fails the build.
        match self.host.build_start().await {
            Ok(chunks) => forward_emitted_chunks(ctx, &chunks, &self.emit),
            Err(e) => return Err(anyhow::anyhow!("plugin buildStart failed:\n{e}")),
        }
        Ok(())
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let host = Arc::clone(&self.host);
        let spec = args.specifier.to_string();
        let importer = args.importer.unwrap_or("").to_string();
        async move {
            host.resolve_id(&spec, &importer)
                .await
                .map(|r| r.map(HookResolveIdOutput::from_id))
                .map_err(|e| anyhow::anyhow!("plugin resolveId failed for {spec}:\n{e}"))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let host = Arc::clone(&self.host);
        let id = args.id.to_string();
        async move {
            // A throwing plugin `load`/`transform` fails the build, as in Vite;
            // bundling the raw source instead would ship wrong output silently.
            host.load(&id)
                .await
                .map_err(|e| anyhow::anyhow!("plugin load failed for {id}:\n{e}"))
                .map(|loaded| loaded.map(|code| {
                    let path = id.split(['?', '#']).next().unwrap_or(&id);
                    if path.ends_with(".css") {
                        // A plugin-served virtual CSS module (e.g. UnoCSS's layer
                        // placeholder): its real CSS is routed through the
                        // `vite:css-post` shim, so keep an empty side-effect stub
                        // in the graph (so the plugin's build hook still sees the
                        // module) rather than parsing CSS as JS.
                        return HookLoadOutput {
                            code: arcstr::ArcStr::from("export {};\n"),
                            module_type: Some(rolldown_common::ModuleType::Js),
                            side_effects: Some(
                                rolldown_common::side_effects::HookSideEffects::NoTreeshake,
                            ),
                            ..Default::default()
                        };
                    }
                    HookLoadOutput {
                        code: arcstr::ArcStr::from(code),
                        // Infer the type from the id so a plugin-served virtual
                        // module keeps its JSX/TS semantics (e.g. unplugin-icons'
                        // `~icons/*.jsx`); a hardcoded Js made oxc parse the JSX
                        // as plain JS and fail.
                        module_type: Some(module_type_for_id(&id)),
                        ..Default::default()
                    }
                }))
        }
    }

    fn transform(
        &self,
        ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let host = Arc::clone(&self.host);
        let emit = Arc::clone(&self.emit);
        let code = args.code.to_string();
        let id = args.id.to_string();
        async move {
            // A `.svg` is componentized (or left as an asset) in the load hook, where
            // the plugin transform chain already ran on the raw markup. Re-running it
            // here would feed svgr its own component output (or an `export default`
            // asset stub) and corrupt it, so skip svg ids.
            if id.split('?').next().unwrap_or(&id).ends_with(".svg") {
                return Ok(None);
            }
            match host.transform(&code, &id, "{}").await {
                Ok((out, _, _, chunks)) => {
                    forward_emitted_chunks(&ctx.inner, &chunks, &emit);
                    if out != code {
                        Ok(Some(HookTransformOutput {
                            code: Some(out),
                            ..Default::default()
                        }))
                    } else {
                        Ok(None)
                    }
                }
                Err(e) => Err(anyhow::anyhow!("plugin transform failed for {id}:\n{e}")),
            }
        }
    }

    async fn generate_bundle(
        &self,
        ctx: &PluginContext,
        args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        // Resolve the final hashed name of every chunk the plugin emitted and
        // seed them into the sidecar so `this.getFileName(refId)` (read here in
        // generateBundle) returns the real output filename.
        let refs: Vec<(String, String)> = self
            .emit
            .chunk_refs
            .lock()
            .unwrap()
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        let direct: Vec<(String, String)> = self
            .emit
            .direct_names
            .lock()
            .unwrap()
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        if !refs.is_empty() || !direct.is_empty() {
            let mut map = serde_json::Map::new();
            for (oj_ref, rd_ref) in refs {
                if let Ok(name) = ctx.get_file_name(&rd_ref) {
                    map.insert(oj_ref, serde_json::Value::String(name.to_string()));
                }
            }
            // Emitted `.html` pages resolve directly to their output path.
            for (oj_ref, name) in direct {
                map.insert(oj_ref, serde_json::Value::String(name));
            }
            if !map.is_empty() {
                let _ = self
                    .host
                    .seed_chunk_names(&serde_json::Value::Object(map).to_string())
                    .await;
            }
        }
        if !self.host.has_generate_bundle().await {
            return Ok(());
        }
        let bundle_json = serialize_bundle_with_vite_manifest(args.bundle, &self.emit.root);
        let mutated = self
            .host
            .generate_bundle(&bundle_json, args.is_write)
            .await
            .map_err(|e| anyhow::anyhow!("plugin generateBundle failed:\n{e}"))?;
        if let Some(mutated) = mutated {
            apply_bundle_mutations(args.bundle, &mutated);
        }
        Ok(())
    }

    fn render_chunk(
        &self,
        _ctx: &PluginContext,
        args: &HookRenderChunkArgs<'_>,
    ) -> impl std::future::Future<Output = HookRenderChunkReturn> + Send {
        let host = Arc::clone(&self.host);
        let enabled = Arc::clone(&self.render_chunk_enabled);
        let code = Arc::clone(&args.code);
        let chunk_json = serialize_rendered_chunk(&args.chunk);
        async move {
            let on = *enabled
                .get_or_init(|| async { host.has_render_chunk().await })
                .await;
            if !on {
                return Ok(None);
            }
            match host.render_chunk(&code, &chunk_json).await {
                Ok(Some(out)) if out != *code => Ok(Some(HookRenderChunkOutput {
                    code: out,
                    map: HookTransformOutputMap::Null,
                })),
                _ => Ok(None),
            }
        }
    }

    async fn write_bundle(
        &self,
        _ctx: &PluginContext,
        args: &mut rolldown_plugin::HookWriteBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        if !self.host.has_write_bundle().await {
            return Ok(());
        }
        let bundle_json = serialize_bundle(args.bundle);
        self.host
            .write_bundle(&bundle_json, true)
            .await
            .map_err(|e| anyhow::anyhow!("plugin writeBundle failed:\n{e}"))
    }

    async fn render_start(
        &self,
        _ctx: &PluginContext,
        _args: &rolldown_plugin::HookRenderStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        self.host
            .render_start()
            .await
            .map_err(|e| anyhow::anyhow!("plugin renderStart failed:\n{e}"))
    }

    async fn close_bundle(
        &self,
        _ctx: &PluginContext,
        _args: Option<&rolldown_plugin::HookCloseBundleArgs<'_>>,
    ) -> rolldown_plugin::HookNoopReturn {
        self.host
            .close_bundle()
            .await
            .map_err(|e| anyhow::anyhow!("plugin closeBundle failed:\n{e}"))
    }
}

fn serialize_rendered_chunk(chunk: &rolldown_common::RollupRenderedChunk) -> String {
    // `chunk.modules` (keyed by module id) is read by renderChunk hooks such as
    // UnoCSS's `unocss:global:build:generate` to find their virtual layer module.
    let mut modules = serde_json::Map::new();
    for (id, rm) in chunk.modules.keys.iter().zip(chunk.modules.values.iter()) {
        modules.insert(
            id.as_str().to_string(),
            serde_json::json!({
                "renderedExports": rm
                    .rendered_exports
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>(),
            }),
        );
    }
    serde_json::json!({
        "type": "chunk",
        "fileName": chunk.filename.to_string(),
        "name": chunk.name.to_string(),
        "isEntry": chunk.is_entry,
        "isDynamicEntry": chunk.is_dynamic_entry,
        "facadeModuleId": chunk.facade_module_id.as_ref().map(|f| f.as_str().to_string()),
        "moduleIds": chunk.module_ids.iter().map(|m| m.as_str().to_string()).collect::<Vec<_>>(),
        "modules": serde_json::Value::Object(modules),
        "imports": chunk.imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        "dynamicImports": chunk.dynamic_imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        "exports": chunk.exports.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    })
    .to_string()
}

fn serialize_bundle(bundle: &[rolldown_common::Output]) -> String {
    use rolldown_common::{Output, StrOrBytes};
    let mut map = serde_json::Map::new();
    for out in bundle {
        match out {
            Output::Chunk(c) => {
                // Rollup's `chunk.modules` is `{ [id]: RenderedModule }`; plugins
                // like @crxjs walk its keys (module ids) and read renderedExports.
                let mut modules = serde_json::Map::new();
                for (id, rm) in c.modules.keys.iter().zip(c.modules.values.iter()) {
                    modules.insert(
                        id.as_str().to_string(),
                        serde_json::json!({
                            "renderedExports": rm
                                .rendered_exports
                                .iter()
                                .map(|e| e.to_string())
                                .collect::<Vec<_>>(),
                        }),
                    );
                }
                map.insert(
                    c.filename.to_string(),
                    serde_json::json!({
                        "type": "chunk",
                        "fileName": c.filename.to_string(),
                        "name": c.name.to_string(),
                        "isEntry": c.is_entry,
                        "isDynamicEntry": c.is_dynamic_entry,
                        "facadeModuleId": c.facade_module_id.as_ref().map(|f| f.as_str().to_string()),
                        "moduleIds": c.module_ids.iter().map(|m| m.as_str().to_string()).collect::<Vec<_>>(),
                        "modules": serde_json::Value::Object(modules),
                        "imports": c.imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                        "dynamicImports": c.dynamic_imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                        "exports": c.exports.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
                        "code": c.code,
                        "map": serde_json::Value::Null,
                        "sourcemapFileName": c.sourcemap_filename,
                    }),
                );
            }
            Output::Asset(a) => {
                let source = match &a.source {
                    StrOrBytes::Str(s) => Some(s.as_str()),
                    StrOrBytes::Bytes(_) => None,
                };
                map.insert(
                    a.filename.to_string(),
                    serde_json::json!({
                        "type": "asset",
                        "fileName": a.filename.to_string(),
                        "name": a.names.first(),
                        "names": a.names,
                        "source": source,
                    }),
                );
            }
        }
    }
    serde_json::Value::Object(map).to_string()
}

fn apply_bundle_mutations(bundle: &mut Vec<rolldown_common::Output>, json: &str) {
    use rolldown_common::{Output, StrOrBytes};
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) else {
        return;
    };
    // A plugin can delete a bundle entry (e.g. @crxjs removes its manifest JS
    // chunk and the input stub): any output whose filename no longer appears in
    // the returned bundle was removed.
    bundle.retain(|out| {
        let key = match out {
            Output::Chunk(c) => c.filename.as_str(),
            Output::Asset(a) => a.filename.as_str(),
        };
        map.contains_key(key)
    });
    for out in bundle.iter_mut() {
        match out {
            Output::Chunk(c) => {
                let Some(entry) = map.get(c.filename.as_str()) else {
                    continue;
                };
                if let Some(code) = entry.get("code").and_then(|x| x.as_str()) {
                    if c.code != code {
                        Arc::make_mut(c).code = code.to_string();
                    }
                }
                // A plugin can rename an output by mutating its `fileName`
                // (the map key stays the original name).
                if let Some(fname) = entry.get("fileName").and_then(|x| x.as_str()) {
                    if c.filename.as_str() != fname {
                        Arc::make_mut(c).filename = fname.into();
                    }
                }
            }
            Output::Asset(a) => {
                let Some(entry) = map.get(a.filename.as_str()) else {
                    continue;
                };
                if let Some(src) = entry.get("source").and_then(|x| x.as_str()) {
                    if a.source.as_bytes() != src.as_bytes() {
                        Arc::make_mut(a).source = StrOrBytes::Str(src.to_string());
                    }
                }
                if let Some(fname) = entry.get("fileName").and_then(|x| x.as_str()) {
                    if a.filename.as_str() != fname {
                        Arc::make_mut(a).filename = fname.into();
                    }
                }
            }
        }
    }
}

async fn user_plugin_host(
    root: &Path,
    base: &str,
    define: &serde_json::Value,
    environments: &serde_json::Value,
    env_name: &str,
    mode: &str,
) -> Option<Arc<PluginHost>> {
    let (file, plugins_format, label) = match oj_server::plugins::plugin_source(root)? {
        oj_server::plugins::PluginSource::OjPlugins(p) => {
            let label = p.file_name().unwrap().to_string_lossy().into_owned();
            (p, "oj", label)
        }
        oj_server::plugins::PluginSource::ViteConfig(p) => (p, "vite", "vite.config".to_string()),
    };
    let config = serde_json::json!({
        "config": { "root": root.display().to_string(), "base": base, "mode": mode, "command": "build", "define": define, "environments": environments },
        "env": { "command": "build", "mode": mode },
        "environment": { "name": env_name, "mode": "build" },
        "pluginsFormat": plugins_format,
    })
    .to_string();
    match PluginHost::spawn(root, &file, &config).await {
        Ok(host) => {
            println!("oj build ({env_name}): plugins from {label}");
            Some(host)
        }
        Err(e) => {
            eprintln!("oj build ({env_name}): plugin host failed to start: {e}");
            None
        }
    }
}

// `define` entries the plugins' config() hooks returned; Vite merges them into
// config.define, so they join the build's define map (plugin value wins).
async fn plugin_config_defines(host: &Option<Arc<PluginHost>>) -> Vec<(String, String)> {
    match host {
        Some(h) => h.config_defines().await,
        None => Vec::new(),
    }
}

pub async fn build(
    root: PathBuf,
    out: Option<PathBuf>,
    ssr: Option<String>,
    cli_mode: Option<&str>,
    empty_out_dir_flag: bool,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;

    // Vite: `mode = inlineConfig.mode || config.mode || "production"`; when the
    // config file itself names a mode and the CLI did not, the config is loaded
    // again under that mode (its function form and `.env.<mode>` depend on it).
    let mut mode_owned = cli_mode.unwrap_or("production").to_string();
    let mut config = oj_config::load_with(&root, "build", &mode_owned)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if cli_mode.is_none() {
        oj_server::plugins::adopt_vite_config_values_default_mode(&mut config, &root, "build", &mode_owned)
            .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(m) = config.mode.clone().filter(|m| *m != mode_owned) {
            mode_owned = m;
            config = oj_config::load_with(&root, "build", &mode_owned)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            oj_server::plugins::adopt_vite_config_values(&mut config, &root, "build", &mode_owned)
            .map_err(|e| anyhow::anyhow!(e))?;
        }
    } else {
        oj_server::plugins::adopt_vite_config_values(&mut config, &root, "build", &mode_owned)
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    let mode: &str = &mode_owned;
    let build_cfg = config.build.clone().unwrap_or_default();
    let ro_opts = oj_config::rolldown_options(&config);
    let out = out
        .or_else(|| build_cfg.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dist"));
    let out_dir = if out.is_absolute() {
        out
    } else {
        root.join(&out)
    };
    let minify = oj_config::build_minify(&config);
    let sourcemap = oj_config::build_sourcemap(&config);
    let empty_out_dir = if empty_out_dir_flag {
        Some(true)
    } else {
        build_cfg.empty_out_dir
    };
    let loaded_env = oj_env::load(&env_dir_of(&root, &config), mode);
    let env_prefixes = env_prefixes_of(&config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";

    let config_ssr_entry =
        oj_config::build_ssr_entry(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(entry) = ssr.or(config_ssr_entry) {
        return build_ssr_app(
            &root,
            &out_dir,
            &entry,
            mode,
            minify,
            sourcemap,
            build_cfg.prerender.clone(),
            empty_out_dir,
        )
        .await;
    }

    if let Some(lib) = build_cfg.lib.clone() {
        return build_library(&root, &out_dir, &config, lib, mode, minify, sourcemap).await;
    }

    let base = normalize_base(config.base.as_deref().unwrap_or("/"));

    prepare_out_dir(&root, &out_dir, empty_out_dir)?;

    let started = Instant::now();

    // Vite resolves the build entry as `rolldownOptions.input || index.html`,
    // where input is string | string[] | Record<name, path>. Each `.html`
    // entry is processed as a page (its `<script type=module>` become JS
    // inputs and the page is re-emitted); everything else is a direct entry.
    let ro_input = ro_opts.and_then(|r| r.get("input")).filter(|v| !v.is_null());
    let named_inputs: Vec<(String, String)> = match ro_input {
        Some(v) => normalize_input_entries(v),
        None => vec![("index".to_string(), "index.html".to_string())],
    };
    if named_inputs.is_empty() {
        bail!("build.rollupOptions.input resolved to no entries");
    }

    let mut html_docs: Vec<HtmlDoc> = Vec::new();
    let html_inline: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let mut inputs: Vec<InputItem> = Vec::new();
    let mut seen_imports: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut had_html = false;
    let push_input = |abs: &Path,
                      name: String,
                      inputs: &mut Vec<InputItem>,
                      seen: &mut std::collections::HashSet<String>| {
        let rel = abs.strip_prefix(&root).unwrap_or(abs);
        let import = format!("./{}", rel.display().to_string().replace('\\', "/"));
        if seen.insert(import.clone()) {
            inputs.push(InputItem {
                name: Some(name),
                import,
                ..Default::default()
            });
        }
    };
    for (name, rel) in &named_inputs {
        let rel_path = Path::new(rel);
        let abs = if rel_path.is_absolute() {
            rel_path.to_path_buf()
        } else {
            root.join(rel)
        };
        if rel.ends_with(".html") {
            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) if ro_input.is_none() => bail!("no index.html in {}", root.display()),
                Err(e) => {
                    return Err(anyhow::Error::from(e))
                        .with_context(|| format!("build input HTML not found: {}", abs.display()));
                }
            };
            had_html = true;
            let html_dir = abs.parent().unwrap_or(&root).to_path_buf();
            let (content, inline) = externalize_inline_scripts(&content, &abs);
            for (_, id, body) in &inline {
                html_inline.lock().unwrap().insert(id.clone(), body.clone());
            }
            let scripts: Vec<HtmlScript> = module_script_srcs(&content)
                .into_iter()
                .map(|src| {
                    let abs = match inline.iter().find(|(p, _, _)| *p == src) {
                        Some((_, id, _)) => PathBuf::from(id),
                        None => resolve_html_ref(&src, &html_dir, &root),
                    };
                    HtmlScript { src, abs }
                })
                .collect();
            for s in &scripts {
                let stem = s
                    .abs
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .unwrap_or("entry")
                    .to_string();
                push_input(&s.abs, stem, &mut inputs, &mut seen_imports);
            }
            let out_rel = if rel_path.is_absolute() {
                abs.strip_prefix(&root)
                    .unwrap_or(&abs)
                    .display()
                    .to_string()
                    .replace('\\', "/")
            } else {
                rel.replace('\\', "/")
            };
            html_docs.push(HtmlDoc {
                out_rel,
                src_html: content,
                scripts,
                dir: html_dir,
            });
        } else {
            if !abs.exists() {
                bail!("build input not found: {}", abs.display());
            }
            push_input(&abs, name.clone(), &mut inputs, &mut seen_imports);
        }
    }
    if inputs.is_empty() {
        if had_html {
            bail!("index.html has no <script type=\"module\" src=...> entry");
        }
        bail!("build.rollupOptions.input resolved to no entries");
    }

    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let css_split = css_code_split_of(&config);
    let plugin_host = user_plugin_host(
        &root,
        &base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
        mode,
    )
    .await;
    let plugin_defines = plugin_config_defines(&plugin_host).await;
    let emit = Arc::new(EmitState::new(root.to_path_buf()));
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        oj_plugins.push(Arc::new(OjUserPlugin::new(
            Arc::clone(host),
            Arc::clone(&emit),
        )));
    }
    let client_minify = oj_config::environment_build_bool(&config, "client", "minify").unwrap_or(minify);
    let client_define: Vec<(String, String)> = {
        let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
        let mut pairs: Vec<(String, String)> = process_env_defines(&node_env);
        pairs.extend(oj_env::import_meta_env_defines(
            &env, mode, !is_production, &base, &env_prefix_refs,
        ));
        pairs.extend(oj_config::config_defines(&config));
        pairs.extend(oj_config::environment_defines(&config, "client"));
        pairs.extend(plugin_defines);
        pairs
    };
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(&root),
        inline_limit: assets_inline_limit_of(&config),
        client: true,
        css_code_split: css_split,
        resolve: css_resolve_of(&root, &config, "client"),
        host: plugin_host.clone(),
        css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
        css: config.css.clone(),
        html_inline: Arc::clone(&html_inline),
        worker: Some(Arc::new(WorkerBundleOpts {
            config: config.clone(),
            is_production,
            define: client_define.clone(),
            minify: client_minify,
        })),
    }));
    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(inputs),
            transform: transform_options(&config, is_production),
            code_splitting: manual_chunks(ro_opts, &root),
            cwd: Some(root.clone()),
            dir: Some(out_dir.display().to_string()),
            resolve: rolldown_resolve(&root, &config, "client"),
            entry_filenames: Some(
                ro_output_str(ro_opts, "entryFileNames")
                    .unwrap_or_else(|| "assets/[name]-[hash].js".to_string())
                    .into(),
            ),
            chunk_filenames: Some(
                ro_output_str(ro_opts, "chunkFileNames")
                    .unwrap_or_else(|| "assets/[name]-[hash].js".to_string())
                    .into(),
            ),
            asset_filenames: ro_output_str(ro_opts, "assetFileNames").map(Into::into),
            external: {
                let ext = ro_external(ro_opts);
                (!ext.is_empty()).then(|| rolldown::IsExternal::from(ext))
            },
            minify: Some(RawMinifyOptions::Bool(client_minify)),
            sourcemap: env_sourcemap(&config, "client", sourcemap),
            define: Some(client_define.into_iter().collect()),
            ..Default::default()
        })
        .build()
        .map_err(|errs| anyhow::anyhow!("rolldown init failed: {errs:?}"))?;

    let output = bundler.write().await.map_err(|errs| {
        let detail = errs
            .into_vec()
            .iter()
            .map(|e| e.to_diagnostic().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::anyhow!("build failed:\n{detail}")
    })?;
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("close failed:\n{errs:?}"))?;

    let unresolved: Vec<String> = output
        .warnings
        .iter()
        .filter(|warning| warning.kind().to_string() == "UNRESOLVED_IMPORT")
        .map(|warning| warning.to_string())
        .collect();
    if !unresolved.is_empty() {
        // Vite fails the build on an unresolved import and writes nothing. rolldown
        // has already written the bundle by the time its diagnostics are readable, so
        // drop the partial output so a failed build never leaves a broken bundle
        // behind. Only under the same containment rule that emptied the directory at
        // build start: an outDir outside root (or the user's emptyOutDir: false) was
        // never oj's to clear, and a failed build must not start clearing it.
        if out_dir_emptiable(&root, &out_dir, empty_out_dir, false) {
            let _ = empty_dir_keep_git(&out_dir);
            // Nothing written means no directory either (like Vite); remove_dir is
            // non-recursive, so a kept .git simply leaves it in place.
            let _ = fs::remove_dir(&out_dir);
        }
        bail!(
            "build failed: unresolved imports:\n{}\n\nThis is most likely unintended because it can break your application at runtime.\nIf you do want to externalize a module explicitly, add it to `build.rollupOptions.external`.",
            unresolved.join("\n")
        );
    }

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end().await {
            bail!("plugin buildEnd failed:\n{e}");
        }
        // CSS a plugin routed through the vite:css-post shim (e.g. UnoCSS's
        // generated utilities) joins the build's stylesheet output.
        for (id, css) in host.get_plugin_css().await {
            let src = if id.is_empty() {
                root.join("__plugin_css__").display().to_string()
            } else {
                id.split(['?', '#']).next().unwrap_or(&id).to_string()
            };
            collected_css.lock().unwrap().push((src, css));
        }
        match host.emitted_files().await {
            Ok(files) => {
                for file in files {
                    let dest = out_dir.join(&file.file_name);
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&dest, file.source.as_bytes())?;
                }
            }
            Err(e) => eprintln!("oj build: plugin emitFile collection failed: {e}"),
        }
    }

    for warning in &output.warnings {
        if format!("{warning:?}").contains("SOURCEMAP_BROKEN") {
            continue;
        }
        eprintln!("oj build warning: {warning:?}");
    }

    // %VITE_*% / import.meta.env substitution (Vite's htmlEnvHook), applied to
    // each page before the plugin transformIndexHtml below.
    let html_env = {
        let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
        let mut defines = oj_env::import_meta_env_defines(&env, mode, !is_production, &base, &env_prefix_refs);
        defines.extend(oj_config::config_defines(&config));
        oj_env::html_env_map(&defines)
    };
    let mut emitted: Vec<(String, usize)> = Vec::new();
    let has_postcss = oj_server::has_postcss_config(&root);
    let public_dir = config
        .public_dir
        .as_ref()
        .map(|p| root.join(p))
        .unwrap_or_else(|| root.join("public"));
    let link_css_transform_enabled: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::new();
    let link_css_resolve = css_resolve_of(&root, &config, "client");
    let asset_names_pattern = ro_output_str(ro_opts, "assetFileNames");
    let css_asset_opts = CssAssetOpts {
        inline_limit: assets_inline_limit_of(&config),
        asset_names: asset_names_pattern.as_deref(),
        resolve: link_css_resolve.as_ref(),
    };
    let mut manifest_entries: Vec<ManifestEntry> = Vec::new();
    let mut imports_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut entry_files: Vec<String> = Vec::new();
    let mut facade_to_file: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            let filename = chunk.filename.to_string();
            emitted.push((filename.clone(), chunk.code.len()));
            imports_map.insert(
                filename.clone(),
                chunk.imports.iter().map(|i| i.to_string()).collect(),
            );
            // Entry chunks are keyed by their source path; shared/non-entry
            // chunks by `_<filename>` (Vite's manifest shape).
            let src = if chunk.is_entry {
                chunk.facade_module_id.as_ref().and_then(|f| {
                    Path::new(f.as_ref())
                        .strip_prefix(&root)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
            } else {
                None
            };
            let key = src.clone().unwrap_or_else(|| {
                format!(
                    "_{}",
                    Path::new(&filename)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&filename)
                )
            });
            manifest_entries.push(ManifestEntry {
                key,
                name: chunk.name.to_string(),
                file: filename.clone(),
                src,
                is_entry: chunk.is_entry,
                is_dynamic_entry: chunk.is_dynamic_entry,
                imports: chunk.imports.iter().map(|i| i.to_string()).collect(),
                dynamic_imports: chunk
                    .dynamic_imports
                    .iter()
                    .map(|i| i.to_string())
                    .collect(),
                css: Vec::new(),
            });
            if chunk.is_entry {
                entry_files.push(filename.clone());
                if let Some(facade) = &chunk.facade_module_id {
                    facade_to_file.insert(PathBuf::from(facade.as_ref()), filename.clone());
                }
            }
        } else if let rolldown_common::Output::Asset(asset) = asset {
            emitted.push((asset.filename.to_string(), asset.source.as_bytes().len()));
        }
    }

    // Non-split CSS is one combined stylesheet linked from every page.
    let combined_css_name: Option<String> = if !css_split {
        let mut css_entries = collected_css.lock().unwrap().clone();
        if css_entries.is_empty() {
            None
        } else {
            css_entries.sort();
            fs::create_dir_all(out_dir.join("assets"))?;
            let mut seen_assets: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            let combined: String = css_entries
                .into_iter()
                .map(|(src, css)| {
                    let dir = Path::new(&src)
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.to_path_buf());
                    rebase_css_urls(&css, &dir, &out_dir, &base, css_asset_opts, &mut emitted, &mut seen_assets)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let hash = content_hash(combined.as_bytes());
            let css_name = render_asset_name(css_asset_opts.asset_names, "style", &hash, "css");
            fs::write(out_dir.join(&css_name), &combined)?;
            emitted.push((css_name.clone(), combined.len()));
            for entry in &mut manifest_entries {
                if entry.is_entry {
                    entry.css.push(css_name.clone());
                }
            }
            Some(css_name)
        }
    } else {
        None
    };
    // Split CSS is emitted once for the whole bundle; each page then links the
    // stylesheets of its own entry chunks (a page-relative href when the base is
    // relative).
    let chunk_css: Vec<(String, String)> = if css_split {
        emit_split_css_files(
            &output,
            &collected_css,
            &out_dir,
            &root,
            &base,
            css_asset_opts,
            &mut emitted,
            &mut manifest_entries,
        )?
    } else {
        Vec::new()
    };
    let mut all_sync_chunks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(name) = oj_config::ssr_manifest_name(&config) {
        let manifest = ssr_manifest(&output, &root, &base, &chunk_css, &combined_css_name, &imports_map);
        let dest = out_dir.join(&name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, serde_json::to_string_pretty(&manifest)?)?;
    }

    // Pages a plugin emitted as `.html` chunks during the build are rendered
    // here alongside the statically-resolved input pages.
    {
        let mut emitted_pages = emit.html_docs.lock().unwrap();
        html_docs.append(&mut emitted_pages);
    }

    // Page script entries across all documents: the only chunks that get the
    // modulepreload polyfill (a worker or plugin-emitted entry has no document).
    let mut page_entry_files: Vec<String> = Vec::new();
    for doc in &html_docs {
        let mut rewritten_html = oj_env::replace_html_env(&doc.src_html, &html_env);
        let page_base = page_base(&base, &doc.out_rel);
        // Rewrite each `<script src>` to its hashed output chunk and collect
        // this page's own entry chunks (for modulepreload and split CSS).
        let mut doc_entry_files: Vec<String> = Vec::new();
        for s in &doc.scripts {
            if let Some(file) = facade_to_file.get(&s.abs) {
                rewritten_html = rewritten_html.replace(&s.src, &with_base(file, &page_base));
                doc_entry_files.push(file.clone());
            }
        }
        page_entry_files.extend(doc_entry_files.iter().cloned());
        for entry in &doc_entry_files {
            all_sync_chunks.insert(entry.clone());
            all_sync_chunks.extend(transitive_imports(entry, &imports_map));
        }

        // Inject <link rel="modulepreload"> for this page's transitively
        // imported chunks so the browser fetches them in parallel.
        let mut preloads: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for entry in &doc_entry_files {
            for dep in transitive_imports(entry, &imports_map) {
                preloads.insert(dep);
            }
        }
        if !preloads.is_empty() && oj_config::module_preload_links(&config) {
            let links = preloads
                .iter()
                .map(|f| {
                    format!(
                        "<link rel=\"modulepreload\" href=\"{}\" />",
                        with_base(f, &page_base)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            rewritten_html = insert_before_head(&rewritten_html, &links);
        }

        // `<link rel="stylesheet" href>` to a local file (relative or root-absolute)
        // goes through the same pipeline as an imported stylesheet (Sass/PostCSS/
        // Tailwind, lightningcss, url() assets) and is emitted hashed, as in Vite.
        {
            let mut seen_link_assets: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            for href in stylesheet_hrefs(&doc.src_html) {
                let src = resolve_html_ref(&href, &doc.dir, &root);
                if !src.is_file() {
                    continue;
                }
                let src_str = src.to_string_lossy().into_owned();
                let out = compile_stylesheet(
                    &root,
                    &plugin_host,
                    &link_css_transform_enabled,
                    has_postcss,
                    &config.css,
                    &link_css_resolve,
                    &src_str,
                    &src_str,
                )
                .await
                .with_context(|| format!("stylesheet {href} linked from {}", doc.out_rel))?;
                let dir = src.parent().map(Path::to_path_buf).unwrap_or_else(|| root.clone());
                let css = rebase_css_urls(
                    &out.css,
                    &dir,
                    &out_dir,
                    &base,
                    css_asset_opts,
                    &mut emitted,
                    &mut seen_link_assets,
                );
                let hash = content_hash(css.as_bytes());
                let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("style");
                let name = render_asset_name(css_asset_opts.asset_names, &sanitize_asset_name(stem), &hash, "css");
                let dest = out_dir.join(&name);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dest, &css)?;
                emitted.push((name.clone(), css.len()));
                rewritten_html =
                    rewrite_link_hrefs(&rewritten_html, &href, &with_base(&name, &base));
            }
        }

        // Every other asset the page references by attribute (img src/srcset,
        // link icon/manifest href, video/audio/source/track, meta og:image...)
        // is emitted hashed or inlined, and a publicDir reference gets the base
        // prefix, as Vite's build-html plugin does.
        {
            let mut seen_html_assets: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            rewritten_html = rewrite_html_asset_attrs(&rewritten_html, |tag, tag_name, _attr, value, srcset| {
                let mut one = |url: &str| {
                    html_asset_url(
                        tag,
                        tag_name,
                        url,
                        &doc.dir,
                        &root,
                        &public_dir,
                        &out_dir,
                        &page_base,
                        css_asset_opts,
                        &mut emitted,
                        &mut seen_html_assets,
                    )
                };
                if srcset {
                    rewrite_srcset(value, one)
                } else {
                    one(value)
                }
            });
        }

        if let Some(css_name) = &combined_css_name {
            let link = format!(
                "<link rel=\"stylesheet\" href=\"{}\" />",
                with_base(css_name, &page_base)
            );
            rewritten_html = insert_before_head(&rewritten_html, &link);
        } else if css_split {
            let links = split_css_links(&chunk_css, &doc_entry_files, &imports_map, &page_base);
            if !links.is_empty() {
                rewritten_html = insert_before_head(&rewritten_html, &links);
            }
        }

        if let Some(host) = &plugin_host {
            if let Ok(out) = host.transform_index_html(&rewritten_html).await {
                rewritten_html = out;
            }
        }
        let dest = out_dir.join(&doc.out_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, rewritten_html)?;
    }

    // Vite's build import analysis: wrap dynamic imports in `__vitePreload` so a
    // lazy chunk's own dependencies and stylesheets load in parallel (and its
    // CSS is applied before it runs), inject the modulepreload polyfill into
    // page entries, and give async-only chunks a stylesheet fallback.
    apply_preload_helper(
        &output,
        &out_dir,
        &base,
        &imports_map,
        &chunk_css,
        &all_sync_chunks,
        &page_entry_files,
        oj_config::module_preload_polyfill(&config),
    )?;

    fs::create_dir_all(out_dir.join(".vite"))?;
    fs::write(
        out_dir.join(".vite").join("manifest.json"),
        serde_json::to_string_pretty(&build_manifest(&manifest_entries))?,
    )?;

    if build_cfg.copy_public_dir.unwrap_or(true) {
        copy_public_dir(&public_dir, &out_dir)?;
    }

    println!(
        "{} build: {} in {:?}",
        oj_server::oj_brand(),
        out_dir.display(),
        started.elapsed()
    );
    emitted.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, bytes) in emitted.iter().take(12) {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    if emitted.len() > 12 {
        println!("  … and {} more files", emitted.len() - 12);
    }
    Ok(())
}

fn human_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}kB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn module_script_srcs(html: &str) -> Vec<String> {
    scan_attrs(html, "<script", "src")
        .into_iter()
        .filter(|src| oj_server::html_entry_src(src).is_some())
        .collect()
}

#[derive(Debug)]
struct HtmlScript {
    src: String,
    abs: PathBuf,
}

#[derive(Debug)]
struct HtmlDoc {
    out_rel: String,
    src_html: String,
    scripts: Vec<HtmlScript>,
    /// Directory of the source page; relative `<link href>`s resolve against it.
    dir: PathBuf,
}

/// Inline `<script type="module">…</script>` blocks (no `src`): the byte range
/// of the whole element and its body.
fn inline_module_scripts(html: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    for (start, _) in html.match_indices("<script") {
        let Some(tag_end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + tag_end];
        if !tag.contains("type=\"module\"") || tag.contains("src=") || tag.ends_with('/') {
            continue;
        }
        let body_start = start + tag_end + 1;
        let Some(close) = html[body_start..].find("</script>") else {
            continue;
        };
        let body = html[body_start..body_start + close].to_string();
        if body.trim().is_empty() {
            continue;
        }
        out.push((start, body_start + close + "</script>".len(), body));
    }
    out
}

/// Vite's html-proxy: each inline module script becomes its own entry
/// (`<page>?html-proxy&index=N.js`, whose relative imports resolve against the
/// page's directory) and the tag is rewritten to a `src` placeholder that the
/// post-bundle HTML rewrite replaces with the hashed chunk. Returns the rewritten
/// html and `(placeholder src, virtual id, body)` per script.
fn externalize_inline_scripts(html: &str, html_abs: &Path) -> (String, Vec<(String, String, String)>) {
    let blocks = inline_module_scripts(html);
    if blocks.is_empty() {
        return (html.to_string(), Vec::new());
    }
    let mut out = html.to_string();
    let mut entries = Vec::new();
    for (n, (start, end, body)) in blocks.iter().enumerate().rev() {
        let placeholder = format!("/@oj-inline/{n}.js");
        let id = format!("{}?html-proxy&index={n}.js", html_abs.display());
        out.replace_range(*start..*end, &format!("<script type=\"module\" src=\"{placeholder}\"></script>"));
        entries.push((placeholder, id, body.clone()));
    }
    entries.reverse();
    (out, entries)
}

/// `href`s of `<link rel="stylesheet">` tags that name a local file (relative or
/// root-absolute), in document order.
fn stylesheet_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (start, _) in html.match_indices("<link") {
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        // Quoted or unquoted attributes, any case (html_attr is the shared parser).
        if !html_attr(tag, "rel").is_some_and(|v| v.eq_ignore_ascii_case("stylesheet")) {
            continue;
        }
        let Some(href) = html_attr(tag, "href") else {
            continue;
        };
        if href.is_empty()
            || href.starts_with("http://")
            || href.starts_with("https://")
            || href.starts_with("//")
            || href.starts_with("data:")
        {
            continue;
        }
        out.push(href.to_string());
    }
    out
}

fn normalize_input_entries(input: &serde_json::Value) -> Vec<(String, String)> {
    let name_of = |p: &str| {
        Path::new(p)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("index")
            .to_string()
    };
    match input {
        serde_json::Value::String(s) => vec![(name_of(s), s.clone())],
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| (name_of(s), s.to_string()))
            .collect(),
        serde_json::Value::Object(map) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

fn resolve_html_ref(src: &str, html_dir: &Path, root: &Path) -> PathBuf {
    let s = src.trim();
    let s = s.strip_prefix("./").unwrap_or(s);
    let joined = match s.strip_prefix('/') {
        Some(rest) => root.join(rest),
        None => html_dir.join(s),
    };
    // Lexically resolve `..` so a page-relative `../src/x.js` matches the
    // normalized facade id rolldown reports for the chunk (and its `<script>`
    // tag gets rewritten).
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn insert_before_head(html: &str, snippet: &str) -> String {
    match html.find("</head>") {
        Some(i) => format!("{}{}\n{}", &html[..i], snippet, &html[i..]),
        None => format!("{snippet}\n{html}"),
    }
}


/// Rewrite the `href` of every `<link>` whose value is `from`, keeping the tag's
/// own quoting (single, double or none), so a bundled stylesheet link is updated
/// however the page spelled it.
fn rewrite_link_hrefs(html: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(html.len() + to.len());
    let mut last = 0;
    for (start, _) in html.match_indices("<link") {
        if start < last {
            continue;
        }
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        let Some(value) = html_attr(tag, "href") else {
            continue;
        };
        if value != from {
            continue;
        }
        let offset = value.as_ptr() as usize - tag.as_ptr() as usize;
        out.push_str(&html[last..start + offset]);
        out.push_str(to);
        last = start + offset + value.len();
    }
    out.push_str(&html[last..]);
    out
}

/// Vite's `DEFAULT_HTML_ASSET_SOURCES` (assetSource.ts): per element, the
/// attributes holding one asset URL and the ones holding a srcset.
fn html_asset_attrs(tag_name: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match tag_name {
        "audio" | "embed" | "input" | "track" => (&["src"], &[]),
        "img" => (&["src"], &["srcset"]),
        "image" | "use" => (&["href", "xlink:href"], &[]),
        "link" => (&["href"], &["imagesrcset"]),
        "object" => (&["data"], &[]),
        "source" => (&["src"], &["srcset"]),
        "video" => (&["src", "poster"], &[]),
        "meta" => (&["content"], &[]),
        _ => return None,
    })
}

/// Vite only treats `<meta content>` as an asset for these `name`/`property`
/// values (msapplication tiles, twitter:image, Open Graph media).
fn meta_content_is_asset(tag: &str) -> bool {
    const NAMES: [&str; 7] = [
        "msapplication-tileimage",
        "msapplication-square70x70logo",
        "msapplication-square150x150logo",
        "msapplication-wide310x150logo",
        "msapplication-square310x310logo",
        "msapplication-config",
        "twitter:image",
    ];
    const PROPERTIES: [&str; 7] = [
        "og:image",
        "og:image:url",
        "og:image:secure_url",
        "og:audio",
        "og:audio:secure_url",
        "og:video",
        "og:video:secure_url",
    ];
    let matches = |attr: &str, allowed: &[&str]| {
        html_attr(tag, attr)
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| allowed.contains(&v.as_str()))
    };
    matches("name", &NAMES) || matches("property", &PROPERTIES)
}

/// A bare (valueless) attribute such as `vite-ignore`.
fn has_bare_attr(tag: &str, name: &str) -> bool {
    tag.split_ascii_whitespace()
        .skip(1)
        .any(|tok| tok.eq_ignore_ascii_case(name) || tok.get(..name.len() + 1).is_some_and(|p| p.eq_ignore_ascii_case(&format!("{name}="))))
}

/// The `<link rel>` values Vite never inlines as a data URL (html.ts
/// `noInlineLinkRels`): a favicon or manifest must stay a real file.
fn link_rel_forbids_inline(tag: &str) -> bool {
    html_attr(tag, "rel").is_some_and(|rel| {
        rel.split_ascii_whitespace()
            .any(|r| matches!(r.to_ascii_lowercase().as_str(), "icon" | "apple-touch-icon" | "apple-touch-startup-image" | "manifest"))
    })
}

/// Vite's `isCSSRequest`.
fn is_css_request(url: &str) -> bool {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let ext = clean.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    matches!(ext, "css" | "less" | "sass" | "scss" | "styl" | "stylus" | "pcss" | "postcss" | "sss")
}

/// Rewrite the value of each srcset candidate (`url descriptor, ...`) with `f`;
/// unchanged when no candidate changes.
fn rewrite_srcset(value: &str, mut f: impl FnMut(&str) -> Option<String>) -> Option<String> {
    let mut changed = false;
    let candidates: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| {
            let (url, descriptor) = match c.split_once(char::is_whitespace) {
                Some((u, d)) => (u, d.trim()),
                None => (c, ""),
            };
            let url = match f(url) {
                Some(u) => {
                    changed = true;
                    u
                }
                None => url.to_string(),
            };
            if descriptor.is_empty() {
                url
            } else {
                format!("{url} {descriptor}")
            }
        })
        .collect();
    changed.then(|| candidates.join(", "))
}

/// Vite's build-html asset pass (html.ts, `getNodeAssetAttributes`): every
/// element whose attributes reference an asset (img src/srcset, link href/
/// imagesrcset, video src/poster, source, audio, track, object data, use/image
/// href, allowed meta content) gets those values rewritten by `rewrite(tag,
/// tag_name, attr, value)`, keeping the page's own quoting. A `vite-ignore`
/// attribute skips the element and is removed.
fn rewrite_html_asset_attrs(
    html: &str,
    mut rewrite: impl FnMut(&str, &str, &str, &str, bool) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    let bytes = html.as_bytes();
    for (start, _) in html.match_indices('<') {
        if start < last {
            continue;
        }
        let name_end = html[start + 1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '-'))
            .map(|i| start + 1 + i)
            .unwrap_or(html.len());
        if name_end == start + 1 || name_end >= bytes.len() || !matches!(bytes[name_end], b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>') {
            continue;
        }
        let tag_name = html[start + 1..name_end].to_ascii_lowercase();
        let Some((src_attrs, srcset_attrs)) = html_asset_attrs(&tag_name) else {
            continue;
        };
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        // (offset within tag, length, replacement)
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        if has_bare_attr(tag, "vite-ignore") {
            if let Some(pos) = tag.to_ascii_lowercase().find("vite-ignore") {
                let mut cut_start = pos;
                while cut_start > 0 && tag.as_bytes()[cut_start - 1].is_ascii_whitespace() {
                    cut_start -= 1;
                }
                let cut_end = tag[pos..].find(char::is_whitespace).map(|i| pos + i).unwrap_or(tag.len());
                edits.push((cut_start, cut_end - cut_start, String::new()));
            }
        } else if tag_name != "meta" || meta_content_is_asset(tag) {
            for (attrs, srcset) in [(src_attrs, false), (srcset_attrs, true)] {
                for attr in attrs.iter() {
                    let Some(value) = html_attr(tag, attr) else {
                        continue;
                    };
                    if let Some(new) = rewrite(tag, &tag_name, attr, value, srcset) {
                        let offset = value.as_ptr() as usize - tag.as_ptr() as usize;
                        edits.push((offset, value.len(), new));
                    }
                }
            }
        }
        if edits.is_empty() {
            continue;
        }
        edits.sort_by_key(|e| e.0);
        out.push_str(&html[last..start]);
        let mut cursor = 0;
        for (offset, len, new) in edits {
            out.push_str(&tag[cursor..offset]);
            out.push_str(&new);
            cursor = offset + len;
        }
        out.push_str(&tag[cursor..]);
        last = start + end;
    }
    out.push_str(&html[last..]);
    out
}

/// Vite's `isExcludedUrl` for html asset references: fragments, data URLs and
/// external (`scheme://`, `//`) URLs are left alone.
fn is_excluded_html_url(url: &str) -> bool {
    url.is_empty()
        || url.starts_with('#')
        || url.starts_with("data:")
        || url.starts_with("//")
        || url.split_once("://").is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Emit one html-referenced source asset (hashed under `assetFileNames`, or
/// inlined as a data URL when small and inlining is allowed) and return its URL
/// from the page. `seen` dedupes by resolved path across the page's references.
fn emit_html_asset(
    abs: &Path,
    suffix: &str,
    out_dir: &Path,
    page_base: &str,
    opts: CssAssetOpts<'_>,
    no_inline: bool,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    let abs = abs.canonicalize().ok()?;
    if let Some(url) = seen.get(&abs) {
        return Some(format!("{url}{suffix}"));
    }
    let data = std::fs::read(&abs).ok()?;
    let ext = abs.extension().and_then(|s| s.to_str()).unwrap_or("");
    // Vite's shouldInline: never an .html, never a link icon/manifest, never an
    // svg addressed by fragment; otherwise anything under assetsInlineLimit.
    if !no_inline && !ext.eq_ignore_ascii_case("html") && (data.len() as u64) <= opts.inline_limit && !(ext.eq_ignore_ascii_case("svg") && suffix.starts_with('#')) {
        if ext.eq_ignore_ascii_case("svg") {
            return Some(svg_data_url(&String::from_utf8_lossy(&data)));
        }
        return Some(format!("data:{};base64,{}", asset_mime(ext), b64(&data)));
    }
    let hash = content_hash(&data);
    let stem = abs.file_stem().and_then(|s| s.to_str()).unwrap_or("asset");
    let name = render_asset_name(opts.asset_names, &sanitize_asset_name(stem), &hash, ext);
    let dest = out_dir.join(&name);
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(&dest, &data).ok()?;
    emitted.push((name.clone(), data.len()));
    let url = with_base(&name, page_base);
    seen.insert(abs, url.clone());
    Some(format!("{url}{suffix}"))
}

/// The new value for one html asset reference (html.ts asset attribute branch):
/// a `publicDir` file keeps its path under the base; a bundle-relative source
/// file is emitted hashed (or inlined); a stylesheet `<link>` is left to the
/// stylesheet pass; anything else (external, data:, missing) stays as written.
fn html_asset_url(
    tag: &str,
    tag_name: &str,
    value: &str,
    html_dir: &Path,
    root: &Path,
    public_dir: &Path,
    out_dir: &Path,
    page_base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    if is_excluded_html_url(value) {
        return None;
    }
    let cut = value.find(['?', '#']).unwrap_or(value.len());
    let (clean, query) = value.split_at(cut);
    // Vite keeps only a `#fragment` on an emitted asset; the public path keeps its
    // query as written.
    let fragment = query.find('#').map(|i| &query[i..]).unwrap_or("");
    if let Some(rest) = clean.strip_prefix('/') {
        if public_dir.join(rest).is_file() {
            return Some(format!("{}{query}", with_base(clean, page_base)));
        }
    }
    if tag_name == "link"
        && is_css_request(clean)
        && html_attr(tag, "media").is_none()
        && html_attr(tag, "disabled").is_none()
        && !has_bare_attr(tag, "disabled")
    {
        return None;
    }
    let abs = resolve_html_ref(clean, html_dir, root);
    if !abs.is_file() {
        return None;
    }
    let no_inline = tag_name == "link" && link_rel_forbids_inline(tag);
    emit_html_asset(&abs, fragment, out_dir, page_base, opts, no_inline, emitted, seen)
}

fn html_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut cursor = bytes.iter().position(u8::is_ascii_whitespace)?;

    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && bytes[cursor] != b'='
            && bytes[cursor] != b'/'
        {
            cursor += 1;
        }
        if cursor == start {
            cursor += 1;
            continue;
        }
        let attribute = &tag[start..cursor];
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }
        let (value_start, value_end) = if matches!(bytes[cursor], b'\'' | b'"') {
            let quote = bytes[cursor];
            let start = cursor + 1;
            cursor = start;
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
            let end = cursor;
            cursor += usize::from(cursor < bytes.len());
            (start, end)
        } else {
            let start = cursor;
            while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            (start, cursor)
        };
        if attribute.eq_ignore_ascii_case(name) {
            return Some(&tag[value_start..value_end]);
        }
    }

    None
}

fn scan_attrs(html: &str, tag_prefix: &str, attr_name: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (start, _) in html.match_indices(tag_prefix) {
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        if tag_prefix == "<script"
            && !html_attr(tag, "type").is_some_and(|value| value.eq_ignore_ascii_case("module"))
        {
            continue;
        }
        if let Some(value) = html_attr(tag, attr_name) {
            values.push(value.to_string());
        }
    }
    values
}

pub(crate) async fn build_ssr(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    mode: &str,
    sourcemap: oj_config::Sourcemap,
) -> anyhow::Result<()> {
    use rolldown::{IsExternal, Platform};

    let entry_import = if entry.starts_with('.') {
        entry.to_string()
    } else {
        format!("./{entry}")
    };
    let stem = Path::new(entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("server")
        .to_string();

    fs::create_dir_all(out_dir)?;
    let started = Instant::now();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let mut config =
        oj_config::load_with(root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    let loaded_env = oj_env::load(&env_dir_of(root, &config), mode);
    let env_prefixes = env_prefixes_of(&config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";
    let ssr_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &ssr_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "ssr",
        mode,
    )
    .await;
    let plugin_defines = plugin_config_defines(&plugin_host).await;

    // Vite's SSR externalization: dependencies stay external unless
    // `ssr.noExternal` (or a webworker target) bundles them; `ssr.external` wins.
    let externals = Arc::new(oj_config::ssr_externals(&config));
    let external = IsExternal::Fn(Some(Arc::new({
        let externals = Arc::clone(&externals);
        let node_modules = root.join("node_modules");
        move |spec: &str, _importer, is_resolved: bool| {
            let ext = if is_resolved {
                // Fallback for dependencies reached some other way (a plugin
                // resolution, a nested node_modules); kept as their resolved path.
                spec.contains("node_modules")
                    && oj_config::SsrExternals::package_of_path(spec)
                        .is_none_or(|pkg| externals.is_external_pkg(pkg))
            } else {
                // Externalize a dependency by its bare specifier, as Vite does, so the
                // server bundle imports `react`, not an absolute path. Only a name that
                // is an installed package qualifies; an alias or a plugin virtual that
                // merely looks bare resolves and bundles as usual.
                let bare = !spec.starts_with('.')
                    && !spec.starts_with('/')
                    && !spec.starts_with('\0')
                    && !spec.contains(':');
                bare && {
                    let pkg = oj_config::SsrExternals::package_of(spec);
                    externals.is_external_pkg(pkg)
                        && node_modules.join(pkg).join("package.json").is_file()
                }
            };
            Box::pin(async move { Ok(ext) })
        }
    })));

    let emit = Arc::new(EmitState::new(root.to_path_buf()));
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        oj_plugins.push(Arc::new(OjUserPlugin::new(
            Arc::clone(host),
            Arc::clone(&emit),
        )));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(root),
        inline_limit: assets_inline_limit_of(&config),
        client: false,
        css_code_split: false,
        resolve: css_resolve_of(root, &config, "ssr"),
        host: plugin_host.clone(),
        css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
        css: config.css.clone(),
        html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
        worker: None,
    }));
    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some(stem.clone()),
                import: entry_import,
                ..Default::default()
            }]),
            cwd: Some(root.to_path_buf()),
            dir: Some(out_dir.display().to_string()),
            resolve: rolldown_resolve(root, &config, "ssr"),
            transform: transform_options(&config, is_production),
            platform: Some(if externals.webworker() { Platform::Browser } else { Platform::Node }),
            external: Some(external),
            format: Some(OutputFormat::Esm),
            entry_filenames: Some(format!("{stem}.mjs").into()),
            chunk_filenames: Some(format!("{stem}-[hash].mjs").into()),
            minify: Some(RawMinifyOptions::Bool(
                oj_config::environment_build_bool(&config, "ssr", "minify").unwrap_or(false),
            )),
            sourcemap: env_sourcemap(&config, "ssr", sourcemap),
            define: Some({
                let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
                // `keepProcessEnv` defaults to true for a server consumer, so only
                // the webworker target gets Vite's `process.env` -> `{}` defines.
                let mut pairs = if externals.webworker() {
                    process_env_defines(&node_env)
                } else {
                    node_env_defines(&node_env)
                };
                pairs.extend(oj_env::import_meta_env_defines_with(
                    &env,
                    mode,
                    !is_production,
                    &ssr_base,
                    &env_prefix_refs,
                    true,
                ));
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "ssr"));
                pairs.extend(plugin_defines);
                pairs.into_iter().collect()
            }),
            ..Default::default()
        })
        .build()
        .map_err(|errs| anyhow::anyhow!("rolldown init failed: {errs:?}"))?;

    let output = bundler
        .write()
        .await
        .map_err(|errs| anyhow::anyhow!("ssr build failed:\n{errs:?}"))?;
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("ssr close failed:\n{errs:?}"))?;

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end().await {
            bail!("plugin buildEnd failed (ssr):\n{e}");
        }
    }

    let mut emitted: Vec<(String, usize)> = Vec::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(c) = asset {
            emitted.push((c.filename.to_string(), c.code.len()));
        }
    }
    println!(
        "oj build (ssr): {} in {:?}",
        out_dir.display(),
        started.elapsed()
    );
    emitted.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, bytes) in &emitted {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    Ok(())
}

const OJ_SERVER_FNS_JS: &str = r#"const mods = import.meta.glob("./src/**/*.server.*");
const norm = (s) => String(s).replace(/^\.?\/+/, "");
export async function dispatch(url, name, args) {
  const want = norm(url);
  const key = Object.keys(mods).find((k) => norm(k) === want);
  if (!key) throw new Error("oj: no server module " + url);
  const m = await mods[key]();
  const fn = name === "default" ? m.default : m[name];
  if (typeof fn !== "function") throw new Error("oj: no server function " + name + " in " + url);
  return fn(...(Array.isArray(args) ? args : []));
}
"#;

fn has_server_modules(root: &Path) -> bool {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if walk(&p) {
                    return true;
                }
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
                    .iter()
                    .any(|s| n.ends_with(s))
            }) {
                return true;
            }
        }
        false
    }
    walk(&root.join("src"))
}

async fn build_server_fns(root: &Path, out_dir: &Path, mode: &str) -> anyhow::Result<()> {
    use rolldown::{IsExternal, Platform};
    let mut config =
        oj_config::load_with(root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    let node_env =
        oj_env::resolve_node_env(shell_node_env().as_deref(), &oj_env::load(root, mode), "production");
    let is_production = node_env == "production";
    let entry_path = root.join("_oj_server_fns_entry.tsx");
    fs::write(&entry_path, OJ_SERVER_FNS_JS)?;
    let collected: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let ext_rule = Arc::new(oj_config::ssr_externals(&config));
    let external = IsExternal::Fn(Some(Arc::new(move |spec: &str, _i, resolved: bool| {
        let in_node_modules = resolved && spec.contains("node_modules");
        let ext = ext_rule
            .is_external(spec, in_node_modules)
            .unwrap_or(false);
        Box::pin(async move { Ok(ext) })
    })));
    let result = async {
        let mut bundler = BundlerBuilder::default()
            .with_plugins(vec![Arc::new(OjCssPlugin {
                collected: Arc::clone(&collected),
                root: root.to_path_buf(),
                has_postcss: oj_server::has_postcss_config(root),
                inline_limit: 4096,
                client: false,
                resolve: css_resolve_of(root, &config, "ssr"),
                css_code_split: false,
                host: None,
                css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
                css: config.css.clone(),
                html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
                worker: None,
            })])
            .with_options(BundlerOptions {
                input: Some(vec![InputItem {
                    name: Some("_oj_server_fns".to_string()),
                    import: "./_oj_server_fns_entry.tsx".to_string(),
                    ..Default::default()
                }]),
                cwd: Some(root.to_path_buf()),
                dir: Some(out_dir.display().to_string()),
                platform: Some(Platform::Node),
                external: Some(external),
                format: Some(OutputFormat::Esm),
                entry_filenames: Some("_oj_server_fns.mjs".to_string().into()),
                chunk_filenames: Some("_oj_server_fns-[hash].mjs".to_string().into()),
                minify: Some(RawMinifyOptions::Bool(false)),
                transform: transform_options(&config, is_production),
                define: Some({
                    let mut pairs = node_env_defines(&node_env);
                    pairs.push(("import.meta.env.SSR".to_string(), "true".to_string()));
                    pairs.into_iter().collect()
                }),
                ..Default::default()
            })
            .build()
            .map_err(|errs| anyhow::anyhow!("server-fns init failed: {errs:?}"))?;
        bundler
            .write()
            .await
            .map_err(|errs| anyhow::anyhow!("server-fns build failed:\n{errs:?}"))?;
        bundler
            .close()
            .await
            .map_err(|errs| anyhow::anyhow!("server-fns close failed:\n{errs:?}"))?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let _ = fs::remove_file(&entry_path);
    result
}

const PRERENDER_JS: &str = r#"import * as entry from "./entry-server.mjs";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

const CLIENT_JS = "__CLIENT_JS__";
const CLIENT_CSS = "__CLIENT_CSS__";
const serialize = (d) => JSON.stringify(d ?? null).replace(/</g, "\\u003c");
const paths = JSON.parse(process.argv[2] || "[]");
const root = process.cwd();

async function renderFull(url, data) {
  if (typeof entry.renderStream === "function") {
    const stream = await entry.renderStream(url, data);
    const reader = stream.getReader();
    const dec = new TextDecoder();
    let out = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      out += dec.decode(value, { stream: true });
    }
    return out;
  }
  return await entry.render(url, data);
}

for (const url of paths) {
  const data = typeof entry.load === "function" ? await entry.load(url) : null;
  const routeHead = typeof entry.head === "function" ? String(await entry.head(url, data)) : "";
  const body = await renderFull(url, data);
  const html =
    '<!doctype html><html><head><meta charset="utf-8">' +
    routeHead +
    `<script>window.__OJ_DATA__=${serialize(data)}</script>` +
    (CLIENT_CSS ? `<link rel="stylesheet" href="${CLIENT_CSS}">` : "") +
    `<script type="module" src="${CLIENT_JS}"></script></head><body><div id="app">` +
    body +
    "</div></body></html>";
  const file = url === "/" ? "index.html" : join(url.replace(/^\/+/, ""), "index.html");
  const dest = join(root, file);
  await mkdir(dirname(dest), { recursive: true });
  await writeFile(dest, html);
  console.error(`oj prerender: ${url} -> ${file}`);
}
"#;

const SSR_PROD_SERVER: &str = r#"import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, normalize } from "node:path";
import * as entry from "./entry-server.mjs";
import { dispatch as __ojDispatch } from "./_oj_server_fns.mjs";

const root = dirname(fileURLToPath(import.meta.url));
const PORT = process.env.PORT || 5180;
const CLIENT_JS = "__CLIENT_JS__";
const CLIENT_CSS = "__CLIENT_CSS__";
const TAIL = "</div></body></html>";
const TYPES = { ".js": "text/javascript", ".css": "text/css", ".map": "application/json" };
const serialize = (data) => JSON.stringify(data ?? null).replace(/</g, "\\u003c");
const readBody = (req) =>
  new Promise((resolve) => {
    let b = "";
    req.on("data", (c) => (b += c));
    req.on("end", () => resolve(b));
  });

createServer(async (req, res) => {
  const url = req.url.split("?")[0];
  if (req.method === "POST" && url === "/__oj_fn") {
    try {
      const { module, name, args } = JSON.parse(await readBody(req));
      const result = await __ojDispatch(module, name, args);
      res.writeHead(200, { "content-type": "application/json" });
      return void res.end(JSON.stringify(result ?? null));
    } catch (e) {
      res.writeHead(500, { "content-type": "text/plain" });
      return void res.end(String((e && e.stack) || e));
    }
  }
  if (url.startsWith("/assets/")) {
    const file = normalize(join(root, url));
    if (!file.startsWith(root)) return void res.writeHead(403).end();
    try {
      const buf = await readFile(file);
      res.writeHead(200, { "content-type": TYPES[file.slice(file.lastIndexOf("."))] || "application/octet-stream" });
      return void res.end(buf);
    } catch {
      return void res.writeHead(404).end();
    }
  }
  try {
    const wantsData = Boolean(req.headers["oj-loader"]);
    const load = () => (typeof entry.load === "function" ? entry.load(url) : null);
    if (req.method === "POST") {
      if (typeof entry.action === "function") await entry.action(url, await readBody(req));
      if (wantsData) {
        const body = serialize(await load());
        res.writeHead(200, { "content-type": "application/json" });
        return void res.end(body);
      }
      return void res.writeHead(303, { location: url }).end();
    }
    if (wantsData) {
      const body = serialize(await load());
      res.writeHead(200, { "content-type": "application/json" });
      return void res.end(body);
    }
    const data = await load();
    const json = serialize(data);
    const routeHead = typeof entry.head === "function" ? String(await entry.head(url, data)) : "";
    const HEAD =
      '<!doctype html><html><head><meta charset="utf-8">' +
      routeHead +
      `<script>window.__OJ_DATA__=${json}</script>` +
      (CLIENT_CSS ? `<link rel="stylesheet" href="${CLIENT_CSS}">` : "") +
      `<script type="module" src="${CLIENT_JS}"></script></head><body><div id="app">`;
    const stream = await entry.renderStream(url, data);
    res.writeHead(200, { "content-type": "text/html; charset=utf-8", "transfer-encoding": "chunked" });
    res.write(HEAD);
    const reader = stream.getReader();
    const dec = new TextDecoder();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      res.write(dec.decode(value, { stream: true }));
    }
    res.write(TAIL);
    res.end();
  } catch (e) {
    res.writeHead(500, { "content-type": "text/html" }).end(`<pre>${String((e && e.stack) || e)}</pre>`);
  }
}).listen(PORT, () => console.log(`oj ssr server on http://localhost:${PORT}`));
"#;

const SSR_WORKER_ENTRY: &str = r#"import * as entry from "./entry-server.mjs";
import { dispatch as __ojDispatch } from "./_oj_server_fns.mjs";

const CLIENT_JS = "__CLIENT_JS__";
const CLIENT_CSS = "__CLIENT_CSS__";
const serialize = (d) => JSON.stringify(d ?? null).replace(/</g, "\\u003c");
const enc = new TextEncoder();

export default {
  async fetch(request) {
    const url = new URL(request.url).pathname;
    if (request.method === "POST" && url === "/__oj_fn") {
      try {
        const { module, name, args } = await request.json();
        return Response.json(await __ojDispatch(module, name, args));
      } catch (e) {
        return new Response(String((e && e.stack) || e), { status: 500 });
      }
    }
    const wantsData = Boolean(request.headers.get("oj-loader"));
    const load = () => (typeof entry.load === "function" ? entry.load(url) : null);
    if (request.method === "POST") {
      if (typeof entry.action === "function") await entry.action(url, await request.text());
      if (wantsData) return Response.json(await load());
      return new Response(null, { status: 303, headers: { location: url } });
    }
    if (wantsData) return Response.json(await load());
    const data = await load();
    const routeHead = typeof entry.head === "function" ? String(await entry.head(url, data)) : "";
    const HEAD =
      '<!doctype html><html><head><meta charset="utf-8">' +
      routeHead +
      `<script>window.__OJ_DATA__=${serialize(data)}</script>` +
      (CLIENT_CSS ? `<link rel="stylesheet" href="${CLIENT_CSS}">` : "") +
      `<script type="module" src="${CLIENT_JS}"></script></head><body><div id="app">`;
    const stream = await entry.renderStream(url, data);
    const body = new ReadableStream({
      async start(controller) {
        controller.enqueue(enc.encode(HEAD));
        const reader = stream.getReader();
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          controller.enqueue(value);
        }
        controller.enqueue(enc.encode("</div></body></html>"));
        controller.close();
      },
    });
    return new Response(body, { headers: { "content-type": "text/html; charset=utf-8" } });
  },
};
"#;

pub(crate) fn derive_client_entry(root: &Path, server_entry: &str) -> Option<String> {
    let file = Path::new(server_entry).file_name()?.to_str()?;
    if !file.contains("server") {
        return None;
    }
    let client_file = file.replace("server", "client");
    let client_rel = match Path::new(server_entry).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            format!("{}/{}", dir.to_string_lossy(), client_file)
        }
        _ => client_file,
    };
    root.join(&client_rel).is_file().then_some(client_rel)
}

pub(crate) async fn build_ssr_app(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    mode: &str,
    minify: bool,
    sourcemap: oj_config::Sourcemap,
    prerender: Option<Vec<String>>,
    empty_out_dir: Option<bool>,
) -> anyhow::Result<()> {
    prepare_out_dir(root, out_dir, empty_out_dir)?;

    build_ssr(root, out_dir, entry, mode, sourcemap).await?;

    let Some(client_entry) = derive_client_entry(root, entry) else {
        println!("oj build (ssr): server bundle only (no *-client sibling to hydrate)");
        return Ok(());
    };
    let (js, css) = build_client_entry(root, out_dir, &client_entry, mode, minify, sourcemap).await?;

    build_server_fns(root, out_dir, mode).await?;
    if has_server_modules(root) {
        println!(
            "  {:>9}  _oj_server_fns.mjs",
            human_bytes(OJ_SERVER_FNS_JS.len())
        );
    }

    let server = SSR_PROD_SERVER
        .replace("__CLIENT_JS__", &js)
        .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""));
    fs::write(out_dir.join("server.mjs"), server)?;
    println!("  {:>9}  server.mjs", human_bytes(SSR_PROD_SERVER.len()));

    let worker = SSR_WORKER_ENTRY
        .replace("__CLIENT_JS__", &js)
        .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""));
    fs::write(out_dir.join("worker.mjs"), worker)?;
    println!(
        "  {:>9}  worker.mjs (edge)",
        human_bytes(SSR_WORKER_ENTRY.len())
    );

    if let Some(paths) = prerender.filter(|p| !p.is_empty()) {
        let script = PRERENDER_JS
            .replace("__CLIENT_JS__", &js)
            .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""));
        let script_path = out_dir.join("_oj_prerender.mjs");
        fs::write(&script_path, script)?;
        let out = std::process::Command::new("node")
            .arg(&script_path)
            .arg(serde_json::to_string(&paths)?)
            .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(root))
            .current_dir(out_dir)
            .output()
            .context("node not found for prerender")?;
        let _ = fs::remove_file(&script_path);
        if !out.status.success() {
            bail!("prerender failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            println!("  {line}");
        }
    }
    println!("  run: node {}", out_dir.join("server.mjs").display());
    Ok(())
}

async fn build_client_entry(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    mode: &str,
    minify: bool,
    sourcemap: oj_config::Sourcemap,
) -> anyhow::Result<(String, Option<String>)> {
    let entry_import = if entry.starts_with('.') {
        entry.to_string()
    } else {
        format!("./{entry}")
    };
    let stem = Path::new(entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("client")
        .to_string();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let mut config =
        oj_config::load_with(root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    let loaded_env = oj_env::load(&env_dir_of(root, &config), mode);
    let env_prefixes = env_prefixes_of(&config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";
    let client_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &client_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
        mode,
    )
    .await;
    let plugin_defines = plugin_config_defines(&plugin_host).await;
    let emit = Arc::new(EmitState::new(root.to_path_buf()));
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        oj_plugins.push(Arc::new(OjUserPlugin::new(
            Arc::clone(host),
            Arc::clone(&emit),
        )));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(root),
        inline_limit: assets_inline_limit_of(&config),
        client: true,
        css_code_split: false,
        resolve: css_resolve_of(root, &config, "client"),
        host: plugin_host.clone(),
        css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
        css: config.css.clone(),
        html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
        worker: None,
    }));

    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some(stem),
                import: entry_import,
                ..Default::default()
            }]),
            cwd: Some(root.to_path_buf()),
            dir: Some(out_dir.display().to_string()),
            resolve: rolldown_resolve(root, &config, "client"),
            transform: transform_options(&config, is_production),
            entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
            chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
            minify: Some(RawMinifyOptions::Bool(
                oj_config::environment_build_bool(&config, "client", "minify").unwrap_or(minify),
            )),
            sourcemap: env_sourcemap(&config, "client", sourcemap),
            define: Some({
                let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
                let mut pairs = process_env_defines(&node_env);
                pairs.extend(oj_env::import_meta_env_defines(
                    &env,
                    mode,
                    !is_production,
                    "/",
                    &env_prefix_refs,
                ));
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "client"));
                pairs.extend(plugin_defines);
                pairs.into_iter().collect()
            }),
            ..Default::default()
        })
        .build()
        .map_err(|errs| anyhow::anyhow!("rolldown init failed: {errs:?}"))?;

    let output = bundler
        .write()
        .await
        .map_err(|errs| anyhow::anyhow!("client build failed:\n{errs:?}"))?;
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("client close failed:\n{errs:?}"))?;

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end().await {
            bail!("plugin buildEnd failed (client):\n{e}");
        }
    }

    let mut js = None;
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(c) = asset {
            if c.is_entry {
                js = Some(format!("/{}", c.filename));
            }
        }
    }
    let js = js.ok_or_else(|| anyhow::anyhow!("client build produced no entry chunk"))?;

    let mut css_entries = collected_css.lock().unwrap().clone();
    let css = if css_entries.is_empty() {
        None
    } else {
        css_entries.sort();
        let combined: String = css_entries
            .into_iter()
            .map(|(_, css)| css)
            .collect::<Vec<_>>()
            .join("\n");
        let hash = format!("{:016x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            combined.hash(&mut h);
            h.finish()
        });
        let name = format!("assets/style-{}.css", &hash[..8]);
        fs::write(out_dir.join(&name), combined)?;
        Some(format!("/{name}"))
    };
    Ok((js, css))
}

async fn build_library(
    root: &Path,
    out_dir: &Path,
    config: &oj_config::OjConfig,
    lib: oj_config::LibConfig,
    mode: &str,
    minify: bool,
    sourcemap: oj_config::Sourcemap,
) -> anyhow::Result<()> {
    let loaded_env = oj_env::load(&env_dir_of(root, config), mode);
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";
    let env_prefixes = env_prefixes_of(config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let import_of = |p: &str| if p.starts_with('.') { p.to_string() } else { format!("./{p}") };
    let stem_of = |p: &str| {
        Path::new(p)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("index")
            .to_string()
    };
    // Vite: a string entry is named by its stem, a list by each stem, an object
    // by its aliases (`resolveRolldownOptions`).
    let entries: Vec<(String, String)> = match &lib.entry {
        oj_config::LibEntry::One(p) => vec![(stem_of(p), import_of(p))],
        oj_config::LibEntry::Many(ps) => ps.iter().map(|p| (stem_of(p), import_of(p))).collect(),
        oj_config::LibEntry::Named(m) => m.iter().map(|(k, p)| (k.clone(), import_of(p))).collect(),
    };
    if entries.is_empty() {
        bail!("build.lib.entry resolved to no entries");
    }
    let multiple = entries.len() > 1;
    // Vite's `resolveLibFilename`: `fileName`, else the (unscoped) package.json
    // name for a single string entry, else the entry's own name; the extension
    // follows the package `type` (`resolveOutputJsExtension`).
    let pkg: Option<serde_json::Value> = fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let pkg_type_module = pkg
        .as_ref()
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == "module");
    let pkg_name = pkg
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|n| n.strip_prefix('@').and_then(|s| s.split_once('/')).map(|(_, rest)| rest).unwrap_or(n).to_string());
    let single_string_entry = matches!(lib.entry, oj_config::LibEntry::One(_));
    let base_name: Option<String> = match (&lib.file_name, multiple) {
        (Some(f), false) => Some(f.clone()),
        (Some(_), true) => {
            eprintln!("oj build: (!) build.lib.fileName is ignored with multiple entries; each entry keeps its own name");
            None
        }
        (None, false) if single_string_entry && pkg.is_some() => Some(
            pkg_name
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Name in package.json is required if option \"build.lib.fileName\" is not provided."))?,
        ),
        (None, false) => Some(entries[0].0.clone()),
        (None, true) => None,
    };
    let js_ext = |fmt: &str| -> &'static str {
        if pkg_type_module {
            if matches!(fmt, "cjs" | "umd") { "cjs" } else { "js" }
        } else if matches!(fmt, "es" | "esm") {
            "mjs"
        } else {
            "js"
        }
    };
    // Vite's `resolveBuildOutputs`: default formats are es+umd for one entry and
    // es+cjs for several; umd/iife need a single entry and `build.lib.name`.
    let formats = lib.formats.clone().unwrap_or_else(|| {
        if multiple { vec!["es".into(), "cjs".into()] } else { vec!["es".into(), "umd".into()] }
    });
    for fmt in &formats {
        if lib_format(fmt).is_none() {
            bail!("unknown lib format: {fmt} (es, cjs, umd, iife)");
        }
        if matches!(fmt.as_str(), "umd" | "iife") {
            if multiple {
                bail!("Multiple entry points are not supported when output formats include \"umd\" or \"iife\".");
            }
            if lib.name.is_none() {
                bail!("Option \"build.lib.name\" is required when output formats include \"umd\" or \"iife\".");
            }
        }
    }

    prepare_out_dir(root, out_dir, config.build.as_ref().and_then(|b| b.empty_out_dir))?;
    let started = Instant::now();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut emitted: Vec<(String, usize)> = Vec::new();

    for fmt in &formats {
        let format = match fmt.as_str() {
            "es" | "esm" => OutputFormat::Esm,
            "cjs" => OutputFormat::Cjs,
            "umd" => OutputFormat::Umd,
            _ => OutputFormat::Iife,
        };
        let ext = js_ext(fmt);
        // `name.js` / `name.cjs` for es and cjs, `name.umd.js` / `name.iife.js`
        // otherwise (Vite `resolveLibFilename`).
        let name_pattern = base_name.clone().unwrap_or_else(|| "[name]".to_string());
        let entry_file = if matches!(fmt.as_str(), "es" | "esm" | "cjs") {
            format!("{name_pattern}.{ext}")
        } else {
            format!("{name_pattern}.{fmt}.{ext}")
        };
        // An ES library keeps its whitespace so `/* @__PURE__ */` annotations
        // survive for the consumer's tree-shaking (Vite `codegen: false`).
        let minify_opts = if minify && matches!(fmt.as_str(), "es" | "esm") {
            RawMinifyOptions::Object(rolldown_common::RawMinifyOptionsDetailed {
                mangle: Some(rolldown_common::RawMangleOptions::default()),
                compress: Some(rolldown_common::RawCompressOptions::default()),
                remove_whitespace: false,
            })
        } else {
            RawMinifyOptions::Bool(minify)
        };

        let mut bundler = BundlerBuilder::default()
            .with_plugins(vec![Arc::new(OjCssPlugin {
                collected: Arc::clone(&collected_css),
                root: root.to_path_buf(),
                has_postcss: oj_server::has_postcss_config(root),
                inline_limit: 4096,
                client: true,
                resolve: css_resolve_of(root, &config, "client"),
                css_code_split: false,
                host: None,
                css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
                css: config.css.clone(),
                html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
                worker: None,
            })])
            .with_options(BundlerOptions {
                input: Some(
                    entries
                        .iter()
                        .map(|(name, import)| InputItem {
                            name: Some(name.clone()),
                            import: import.clone(),
                            ..Default::default()
                        })
                        .collect(),
                ),
                cwd: Some(root.to_path_buf()),
                dir: Some(out_dir.display().to_string()),
                resolve: rolldown_resolve(root, config, "client"),
                format: Some(format),
                name: lib.name.clone(),
                entry_filenames: Some(entry_file.into()),
                chunk_filenames: Some(format!("[name]-[hash].{ext}").into()),
                asset_filenames: Some("[name].[ext]".to_string().into()),
                code_splitting: matches!(fmt.as_str(), "umd" | "iife")
                    .then_some(rolldown_common::CodeSplittingMode::Bool(false)),
                minify: Some(minify_opts),
                sourcemap: sourcemap_type(sourcemap),
                transform: transform_options(config, is_production),
                // Vite's define plugin adds no `process.env` defines for a library
                // (rolldown's own browser-platform NODE_ENV default still applies,
                // as under Vite); import.meta.env and the user's `define` do.
                define: Some({
                    let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
                    let mut pairs = oj_env::import_meta_env_defines(&env, mode, !is_production, "/", &env_prefix_refs);
                    pairs.extend(oj_config::config_defines(config));
                    pairs.extend(oj_config::environment_defines(config, "client"));
                    pairs.into_iter().collect()
                }),
                ..Default::default()
            })
            .build()
            .map_err(|errs| anyhow::anyhow!("rolldown init failed: {errs:?}"))?;

        let output = bundler
            .write()
            .await
            .map_err(|errs| anyhow::anyhow!("lib build ({fmt}) failed:\n{errs:?}"))?;
        for asset in &output.assets {
            if let rolldown_common::Output::Chunk(c) = asset {
                emitted.push((c.filename.to_string(), c.code.len()));
            }
        }
    }

    let css_entries = collected_css.lock().unwrap().clone();
    if !css_entries.is_empty() {
        let combined: String = css_entries
            .into_iter()
            .map(|(_, css)| css)
            .collect::<Vec<_>>()
            .join("\n");
        // Vite: `build.lib.cssFileName`, else the library's file name / package name.
        let css_stem = lib
            .css_file_name
            .clone()
            .or_else(|| base_name.clone())
            .or_else(|| pkg_name.clone())
            .unwrap_or_else(|| "style".to_string());
        let css_name = format!("{css_stem}.css");
        fs::write(out_dir.join(&css_name), &combined)?;
        emitted.push((css_name, combined.len()));
    }

    println!(
        "oj build (library): {} in {:?}",
        out_dir.display(),
        started.elapsed()
    );
    emitted.sort_by(|a, b| b.1.cmp(&a.1));
    emitted.dedup();
    for (name, bytes) in &emitted {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    Ok(())
}

/// The library formats Vite accepts; `true` when the format needs `build.lib.name`.
fn lib_format(fmt: &str) -> Option<bool> {
    match fmt {
        "es" | "esm" | "cjs" => Some(false),
        "umd" | "iife" => Some(true),
        _ => None,
    }
}

/// Vite's `base`: `""` and `"./"` mean a relative base (URLs are emitted relative
/// to the page / stylesheet / chunk that references them, so the build can be
/// served from any path or from `file://`); anything else is an absolute prefix.
fn normalize_base(base: &str) -> String {
    if base.is_empty() || base == "./" {
        return "./".to_string();
    }
    let mut b = base.to_string();
    if !b.starts_with('/') {
        b.insert(0, '/');
    }
    if !b.ends_with('/') {
        b.push('/');
    }
    b
}

fn is_relative_base(base: &str) -> bool {
    base == "./"
}

/// A public URL for an output file from a root-level page (or any consumer that
/// sits at the outDir root).
fn with_base(filename: &str, base: &str) -> String {
    format!("{base}{}", filename.trim_start_matches('/'))
}

/// The base to use from a page at `out_rel` (e.g. `nested/index.html`): with a
/// relative base that is `../` per directory level, otherwise the absolute base.
fn page_base(base: &str, out_rel: &str) -> String {
    if !is_relative_base(base) {
        return base.to_string();
    }
    let depth = out_rel.trim_start_matches('/').matches('/').count();
    if depth == 0 {
        "./".to_string()
    } else {
        "../".repeat(depth)
    }
}

/// URL for an emitted `assets/…` file as referenced from a stylesheet that also
/// lives in `assets/` (all of oj's emitted CSS does): relative bases become a
/// sibling reference, absolute bases the public path.
fn css_asset_url(name: &str, base: &str) -> String {
    if is_relative_base(base) {
        format!("./{}", name.strip_prefix("assets/").unwrap_or(name))
    } else {
        with_base(name, base)
    }
}

/// `to` relative to the directory of `from` (both outDir-relative, `/`-separated).
fn relative_chunk_path(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from.rsplit_once('/').map(|(d, _)| d.split('/').collect()).unwrap_or_default();
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_dir
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::new();
    for _ in common..from_dir.len() {
        out.push_str("../");
    }
    if out.is_empty() {
        out.push_str("./");
    }
    out.push_str(&to_parts[common..].join("/"));
    out
}

/// All chunks reachable from `entry` via static imports (excludes `entry`).
fn transitive_imports(
    entry: &str,
    map: &std::collections::HashMap<String, Vec<String>>,
) -> std::collections::BTreeSet<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = map.get(entry).cloned().unwrap_or_default();
    while let Some(f) = stack.pop() {
        if seen.insert(f.clone()) {
            if let Some(deps) = map.get(&f) {
                stack.extend(deps.iter().cloned());
            }
        }
    }
    seen
}

fn sanitize_asset_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

/// Emit a CSS-referenced asset (font/image) under a content hash and return its
/// base-prefixed URL, or None to leave the `url()` untouched (data:/absolute/external).
/// How files oj writes itself (CSS url() assets, chunk stylesheets) are named
/// and when they are inlined: `build.assetsInlineLimit` and
/// `build.rollupOptions.output.assetFileNames`.
#[derive(Debug, Clone, Copy)]
struct CssAssetOpts<'a> {
    inline_limit: u64,
    asset_names: Option<&'a str>,
    /// Alias and root-absolute resolution for `url()` specifiers.
    resolve: oj_css::CssResolve<'a>,
}

/// Render Vite/rolldown `assetFileNames` placeholders (`[name]`, `[hash]`,
/// `[ext]`, `[extname]`); the default is Vite's `assets/[name]-[hash][extname]`.
fn render_asset_name(pattern: Option<&str>, name: &str, hash: &str, ext: &str) -> String {
    let pattern = pattern.unwrap_or("assets/[name]-[hash][extname]");
    let extname = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    pattern
        .replace("[name]", name)
        .replace("[hash]", &hash[..hash.len().min(8)])
        .replace("[extname]", &extname)
        .replace("[ext]", ext)
}

fn emit_css_url(
    inner: &str,
    css_dir: &Path,
    out_dir: &Path,
    base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    if inner.is_empty()
        || inner.starts_with("data:")
        || inner.starts_with("http://")
        || inner.starts_with("https://")
        || inner.starts_with("//")
        || inner.starts_with('#')
    {
        return None;
    }
    let cut = inner.find(['?', '#']).unwrap_or(inner.len());
    let (clean, suffix) = inner.split_at(cut);
    // Vite's url resolver: a public-dir file keeps its url; an alias or a
    // root-absolute path names a file that is emitted like a relative one.
    let abs = if let Some(aliased) = opts.resolve.alias_path(clean) {
        aliased
    } else if let Some(rel) = clean.strip_prefix('/') {
        if rel.is_empty() || opts.resolve.public_file(clean).is_some() {
            return None;
        }
        opts.resolve.root_path(clean)?
    } else {
        css_dir.join(clean)
    };
    let abs = abs.canonicalize().ok()?;
    if let Some(url) = seen.get(&abs) {
        return Some(format!("{url}{suffix}"));
    }
    let data = std::fs::read(&abs).ok()?;
    let ext = abs.extension().and_then(|s| s.to_str()).unwrap_or("");
    // Small assets referenced from CSS are inlined as in Vite (assetsInlineLimit),
    // except an svg with a fragment, which must stay a file to keep its `#id`.
    if (data.len() as u64) <= opts.inline_limit && !suffix.starts_with('#') {
        if ext.eq_ignore_ascii_case("svg") {
            return Some(svg_data_url(&String::from_utf8_lossy(&data)));
        }
        return Some(format!("data:{};base64,{}", asset_mime(ext), b64(&data)));
    }
    let hash = content_hash(&data);
    let stem = abs.file_stem().and_then(|s| s.to_str()).unwrap_or("asset");
    let name = render_asset_name(opts.asset_names, &sanitize_asset_name(stem), &hash, ext);
    let dest = out_dir.join(&name);
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(&dest, &data).ok()?;
    emitted.push((name.clone(), data.len()));
    let url = css_asset_url(&name, base);
    seen.insert(abs, url.clone());
    Some(format!("{url}{suffix}"))
}

/// Rewrite relative `url()` refs in one stylesheet to point at emitted,
/// content-hashed assets, since the stylesheet is concatenated into
/// `/assets/style-*.css` where the original relative paths would 404.
fn rebase_css_urls(
    css: &str,
    css_dir: &Path,
    out_dir: &Path,
    base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(pos) = rest.find("url(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        let Some(close) = after.find(')') else {
            out.push_str("url(");
            rest = after;
            continue;
        };
        let inner_raw = &after[..close];
        let inner = inner_raw
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim();
        match emit_css_url(inner, css_dir, out_dir, base, opts, emitted, seen) {
            Some(url) => {
                out.push_str("url(\"");
                out.push_str(&url);
                out.push_str("\")");
            }
            None => {
                out.push_str("url(");
                out.push_str(inner_raw);
                out.push(')');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `build.cssCodeSplit`: emit one stylesheet per chunk that imports CSS and
/// return `(chunk filename, css filename)` pairs. Which pages link which
/// stylesheet render-blocking is decided per page by `split_css_links`; chunks
/// only reached through dynamic imports get their CSS from `__vitePreload` (and
/// a self-injecting fallback, see `apply_preload_helper`).
fn emit_split_css_files(
    output: &rolldown::BundleOutput,
    collected_css: &Arc<Mutex<Vec<(String, String)>>>,
    out_dir: &Path,
    root: &Path,
    base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    manifest_entries: &mut [ManifestEntry],
) -> anyhow::Result<Vec<(String, String)>> {
    let css_map: std::collections::HashMap<String, String> =
        collected_css.lock().unwrap().iter().cloned().collect();
    let mut chunk_css: Vec<(String, String)> = Vec::new();
    if css_map.is_empty() {
        return Ok(chunk_css);
    }
    fs::create_dir_all(out_dir.join("assets"))?;
    let mut seen_assets: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    for asset in &output.assets {
        let rolldown_common::Output::Chunk(chunk) = asset else {
            continue;
        };
        let mut css = String::new();
        for module_id in &chunk.modules.keys {
            let mid = module_id.to_string();
            if let Some(src) = css_map.get(&mid) {
                let dir = Path::new(&mid)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf());
                if !css.is_empty() {
                    css.push('\n');
                }
                css.push_str(&rebase_css_urls(
                    src,
                    &dir,
                    out_dir,
                    base,
                    opts,
                    emitted,
                    &mut seen_assets,
                ));
            }
        }
        if css.is_empty() {
            continue;
        }
        let hash = content_hash(css.as_bytes());
        let css_name = render_asset_name(opts.asset_names, &sanitize_asset_name(&chunk.name), &hash, "css");
        if let Some(parent) = out_dir.join(&css_name).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out_dir.join(&css_name), &css)?;
        emitted.push((css_name.clone(), css.len()));
        for entry in manifest_entries.iter_mut() {
            if entry.file == chunk.filename.as_str() {
                entry.css.push(css_name.clone());
            }
        }
        chunk_css.push((chunk.filename.to_string(), css_name));
    }
    Ok(chunk_css)
}

/// Render-blocking `<link rel=stylesheet>` tags for a page: the CSS of its entry
/// chunks and of every chunk they reach through static imports (no FOUC).
fn split_css_links(
    chunk_css: &[(String, String)],
    entry_files: &[String],
    imports_map: &std::collections::HashMap<String, Vec<String>>,
    page_base: &str,
) -> String {
    let mut sync_chunks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in entry_files {
        sync_chunks.insert(entry.clone());
        sync_chunks.extend(transitive_imports(entry, imports_map));
    }
    let mut linked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut links = String::new();
    for (chunk_file, css_name) in chunk_css {
        if sync_chunks.contains(chunk_file) && linked.insert(css_name.clone()) {
            if !links.is_empty() {
                links.push('\n');
            }
            links.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\" />",
                with_base(css_name, page_base)
            ));
        }
    }
    links
}

/// Vite's `__vitePreload` helper (importAnalysisBuild.ts), ES2015 so it survives
/// any `build.target`. `__vite__assetsURL` is generated per base: a relative base
/// resolves deps against the importing chunk, an absolute base is prefixed.
const PRELOAD_HELPER_JS: &str = r#"const __vite__scriptRel=(function(){var r=typeof document!=="undefined"&&document.createElement("link").relList;return r&&r.supports&&r.supports("modulepreload")?"modulepreload":"preload"})();const __vite__seen={};function __vitePreload(baseModule,deps,importerUrl){var promise=Promise.resolve();if(deps&&deps.length>0){var links=document.getElementsByTagName("link");var cspNonceMeta=document.querySelector("meta[property=csp-nonce]");var cspNonce=cspNonceMeta&&(cspNonceMeta.nonce||cspNonceMeta.getAttribute("nonce"));var allSettled=function(ps){return Promise.all(ps.map(function(p){return Promise.resolve(p).then(function(value){return{status:"fulfilled",value:value}},function(reason){return{status:"rejected",reason:reason}})}))};promise=allSettled(deps.map(function(dep){dep=new URL(__vite__assetsURL(dep,importerUrl),import.meta.url).href;if(dep in __vite__seen)return;__vite__seen[dep]=true;var isCss=dep.endsWith(".css");for(var i=links.length-1;i>=0;i--){var l=links[i];if(l.href===dep&&(!isCss||l.rel==="stylesheet"))return}var link=document.createElement("link");link.rel=isCss?"stylesheet":__vite__scriptRel;if(!isCss){link.as="script"}link.crossOrigin="";link.href=dep;if(cspNonce)link.setAttribute("nonce",cspNonce);document.head.appendChild(link);if(isCss){return new Promise(function(res,rej){link.addEventListener("load",res);link.addEventListener("error",function(){rej(new Error("Unable to preload CSS for "+dep))})})}}))}function handlePreloadError(err){var e=new Event("vite:preloadError",{cancelable:true});e.payload=err;window.dispatchEvent(e);if(!e.defaultPrevented)throw err}return promise.then(function(res){for(var i=0;i<(res||[]).length;i++){var item=res[i];if(item.status!=="rejected")continue;handlePreloadError(item.reason)}return baseModule().catch(handlePreloadError)})}"#;

/// Vite's `vite/modulepreload-polyfill` (fetches `<link rel=modulepreload>` on
/// browsers without native support), injected into page entry chunks.
const MODULEPRELOAD_POLYFILL_JS: &str = r#"(function(){var relList=document.createElement("link").relList;if(relList&&relList.supports&&relList.supports("modulepreload"))return;for(var i=0,ls=document.querySelectorAll('link[rel="modulepreload"]');i<ls.length;i++)processPreload(ls[i]);new MutationObserver(function(mutations){for(var m=0;m<mutations.length;m++){var mutation=mutations[m];if(mutation.type!=="childList")continue;for(var n=0;n<mutation.addedNodes.length;n++){var node=mutation.addedNodes[n];if(node.tagName==="LINK"&&node.rel==="modulepreload")processPreload(node)}}}).observe(document,{childList:true,subtree:true});function getFetchOpts(link){var fetchOpts={};if(link.integrity)fetchOpts.integrity=link.integrity;if(link.referrerPolicy)fetchOpts.referrerPolicy=link.referrerPolicy;if(link.crossOrigin==="use-credentials")fetchOpts.credentials="include";else if(link.crossOrigin==="anonymous")fetchOpts.credentials="omit";else fetchOpts.credentials="same-origin";return fetchOpts}function processPreload(link){if(link.ep)return;link.ep=true;var fetchOpts=getFetchOpts(link);fetch(link.href,fetchOpts)}})();"#;

/// Wrap each `import("./chunk.js")` in a chunk with `__vitePreload`, passing the
/// dependency list Vite would (the lazy chunk, its statically imported chunks
/// and their stylesheets, minus what the importer already has), prepend the
/// modulepreload polyfill to page entries, and give async-only chunks that own
/// CSS a self-injecting fallback link. Everything is prepended on the chunk's
/// first line so existing sourcemap line numbers stay valid.
#[allow(clippy::too_many_arguments)]
fn apply_preload_helper(
    output: &rolldown::BundleOutput,
    out_dir: &Path,
    base: &str,
    imports_map: &std::collections::HashMap<String, Vec<String>>,
    chunk_css: &[(String, String)],
    sync_chunks: &std::collections::BTreeSet<String>,
    page_entries: &[String],
    polyfill: bool,
) -> anyhow::Result<()> {
    let css_of: std::collections::HashMap<&str, &str> = chunk_css
        .iter()
        .map(|(c, css)| (c.as_str(), css.as_str()))
        .collect();
    let relative = is_relative_base(base);
    for asset in &output.assets {
        let rolldown_common::Output::Chunk(chunk) = asset else {
            continue;
        };
        let file = chunk.filename.to_string();
        let path = out_dir.join(&file);
        let Ok(mut code) = fs::read_to_string(&path) else {
            continue;
        };
        let mut prefix = String::new();

        if polyfill && page_entries.iter().any(|e| e == &file) {
            prefix.push_str(MODULEPRELOAD_POLYFILL_JS);
        }

        let mut dep_list: Vec<String> = Vec::new();
        let mut wrapped = false;
        if !chunk.dynamic_imports.is_empty() {
            let own: std::collections::BTreeSet<String> = std::iter::once(file.clone())
                .chain(transitive_imports(&file, imports_map))
                .collect();
            for target in &chunk.dynamic_imports {
                let target = target.to_string();
                let rel = relative_chunk_path(&file, &target);
                let mut deps: Vec<String> = Vec::new();
                let closure: Vec<String> = std::iter::once(target.clone())
                    .chain(transitive_imports(&target, imports_map))
                    .collect();
                for c in &closure {
                    if own.contains(c) {
                        continue;
                    }
                    deps.push(c.clone());
                    if let Some(css) = css_of.get(c.as_str()) {
                        deps.push(css.to_string());
                    }
                }
                // Only the chunk itself: `import()` fetches it anyway.
                if deps.len() <= 1 {
                    deps.clear();
                }
                let mut idx: Vec<String> = Vec::new();
                for d in deps {
                    let d = if relative {
                        relative_chunk_path(&file, &d)
                    } else {
                        d
                    };
                    let i = match dep_list.iter().position(|x| x == &d) {
                        Some(i) => i,
                        None => {
                            dep_list.push(d);
                            dep_list.len() - 1
                        }
                    };
                    idx.push(i.to_string());
                }
                // rolldown's minifier may emit the specifier in any quote style.
                for quote in ['"', '\'', '`'] {
                    let needle = format!("import({quote}{rel}{quote})");
                    if code.contains(&needle) {
                        let replacement = format!(
                            "__vitePreload(function(){{return import({quote}{rel}{quote})}},__vite__mapDeps([{}]),import.meta.url)",
                            idx.join(",")
                        );
                        code = code.replace(&needle, &replacement);
                        wrapped = true;
                    }
                }
            }
        }
        if wrapped {
            let assets_url = if relative {
                "function(dep,importerUrl){return new URL(dep,importerUrl).href}".to_string()
            } else {
                format!(
                    "function(dep){{return {}+dep}}",
                    serde_json::Value::String(base.to_string())
                )
            };
            prefix.push_str(&format!(
                "const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f={})))=>i.map(i=>d[i]);const __vite__assetsURL={assets_url};{PRELOAD_HELPER_JS}",
                serde_json::Value::Array(
                    dep_list
                        .iter()
                        .map(|d| serde_json::Value::String(d.clone()))
                        .collect()
                )
            ));
        }

        // Async-only chunks with CSS: a fallback for dynamic imports the rewrite
        // above could not see (variable specifiers), idempotent against preload.
        if !sync_chunks.contains(&file) {
            if let Some(css_name) = css_of.get(file.as_str()) {
                let href = serde_json::Value::String(if relative {
                    relative_chunk_path(&file, css_name)
                } else {
                    with_base(css_name, base)
                });
                // Compare resolved URLs (`link.href` is absolute), so a link the
                // preload helper or the page already added is recognized.
                prefix.push_str(&format!(
                    "(function(){{var u=new URL({href},import.meta.url).href;var ls=document.getElementsByTagName('link');for(var i=0;i<ls.length;i++){{if(ls[i].rel==='stylesheet'&&ls[i].href===u)return}}var l=document.createElement('link');l.rel='stylesheet';l.href=u;document.head.appendChild(l)}})();"
                ));
            }
        }

        if !prefix.is_empty() {
            fs::write(&path, format!("{prefix}{code}"))?;
        }
    }
    Ok(())
}

/// Vite's `ssr-manifest.json` (ssrManifestPlugin): for every source module in the
/// client bundle, the public URLs a server renderer should preload when that
/// module renders: the module's chunk (non-entry chunks only; entries are already
/// in the HTML) and that chunk's stylesheet. Keys are root-relative paths.
fn ssr_manifest(
    output: &rolldown::BundleOutput,
    root: &Path,
    base: &str,
    chunk_css: &[(String, String)],
    combined_css: &Option<String>,
    imports_map: &std::collections::HashMap<String, Vec<String>>,
) -> serde_json::Map<String, serde_json::Value> {
    let css_of: std::collections::HashMap<&str, &str> = chunk_css
        .iter()
        .map(|(c, css)| (c.as_str(), css.as_str()))
        .collect();
    let mut manifest = serde_json::Map::new();
    for asset in &output.assets {
        let rolldown_common::Output::Chunk(chunk) = asset else {
            continue;
        };
        let file = chunk.filename.to_string();
        // Vite's css deps map: each dynamically imported chunk is keyed by its
        // file name with the stylesheets it and its static imports bring in, so
        // an SSR renderer can preload the CSS of a lazy route.
        for target in &chunk.dynamic_imports {
            let target = target.to_string();
            let mut deps: Vec<serde_json::Value> = Vec::new();
            let mut push_css = |f: &str| {
                if f != file {
                    if let Some(css) = css_of.get(f) {
                        let url: serde_json::Value = with_base(css, base).into();
                        if !deps.contains(&url) {
                            deps.push(url);
                        }
                    }
                }
            };
            push_css(&target);
            for dep in transitive_imports(&target, imports_map) {
                push_css(&dep);
            }
            let key = target.rsplit('/').next().unwrap_or(&target).to_string();
            manifest.insert(key, serde_json::Value::Array(deps));
        }
        let mut urls: Vec<serde_json::Value> = Vec::new();
        if !chunk.is_entry {
            urls.push(with_base(&file, base).into());
            if let Some(css) = css_of.get(file.as_str()) {
                urls.push(with_base(css, base).into());
            }
        } else if let Some(css) = combined_css {
            urls.push(with_base(css, base).into());
        }
        for module_id in &chunk.modules.keys {
            let mid = module_id.to_string();
            let Ok(rel) = Path::new(&mid).strip_prefix(root) else {
                continue;
            };
            let key = rel.to_string_lossy().replace('\\', "/");
            let entry = manifest
                .entry(key)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let Some(list) = entry.as_array_mut() {
                for u in &urls {
                    if !list.contains(u) {
                        list.push(u.clone());
                    }
                }
            }
        }
    }
    manifest
}

struct ManifestEntry {
    /// Manifest key: the entry's `src` path, or `_<filename>` for a shared chunk.
    key: String,
    name: String,
    file: String,
    src: Option<String>,
    is_entry: bool,
    is_dynamic_entry: bool,
    /// Statically-imported chunk filenames (remapped to manifest keys on output).
    imports: Vec<String>,
    dynamic_imports: Vec<String>,
    css: Vec<String>,
}

// Build Vite-manifest entries directly from a bundle (chunks only; CSS is added
// later in the output stage). Used to expose `.vite/manifest.json` inside the
// generateBundle bundle for plugins that read it (e.g. @crxjs).
fn manifest_entries_from_bundle(
    bundle: &[rolldown_common::Output],
    root: &Path,
) -> Vec<ManifestEntry> {
    use rolldown_common::Output;
    let mut entries = Vec::new();
    for out in bundle {
        let Output::Chunk(chunk) = out else { continue };
        let filename = chunk.filename.to_string();
        let src = if chunk.is_entry {
            chunk.facade_module_id.as_ref().and_then(|f| {
                Path::new(f.as_ref())
                    .strip_prefix(root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
        } else {
            None
        };
        let key = src.clone().unwrap_or_else(|| {
            format!(
                "_{}",
                Path::new(&filename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&filename)
            )
        });
        entries.push(ManifestEntry {
            key,
            name: chunk.name.to_string(),
            file: filename,
            src,
            is_entry: chunk.is_entry,
            is_dynamic_entry: chunk.is_dynamic_entry,
            imports: chunk.imports.iter().map(|i| i.to_string()).collect(),
            dynamic_imports: chunk.dynamic_imports.iter().map(|i| i.to_string()).collect(),
            css: Vec::new(),
        });
    }
    entries
}

// serialize_bundle plus a synthetic `.vite/manifest.json` asset, so a plugin's
// generateBundle can read the Vite build manifest from the bundle the way it
// would under Vite (with build.manifest enabled).
fn serialize_bundle_with_vite_manifest(bundle: &[rolldown_common::Output], root: &Path) -> String {
    let base = serialize_bundle(bundle);
    let entries = manifest_entries_from_bundle(bundle, root);
    let manifest = build_manifest(&entries).to_string();
    let mut val: serde_json::Value =
        serde_json::from_str(&base).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            ".vite/manifest.json".to_string(),
            serde_json::json!({
                "type": "asset",
                "fileName": ".vite/manifest.json",
                "name": "manifest.json",
                "source": manifest,
            }),
        );
    }
    val.to_string()
}

fn build_manifest(entries: &[ManifestEntry]) -> serde_json::Value {
    // Vite's manifest references imports/dynamicImports by manifest KEY, not by
    // output filename, so build a filename -> key map first.
    let file_to_key: std::collections::HashMap<&str, &str> = entries
        .iter()
        .map(|e| (e.file.as_str(), e.key.as_str()))
        .collect();
    let remap = |files: &[String]| -> Vec<serde_json::Value> {
        files
            .iter()
            .filter_map(|f| {
                file_to_key
                    .get(f.as_str())
                    .map(|k| serde_json::Value::from(*k))
            })
            .collect()
    };
    let mut map = serde_json::Map::new();
    for e in entries {
        let mut row = serde_json::Map::new();
        row.insert("file".into(), e.file.clone().into());
        row.insert("name".into(), e.name.clone().into());
        if let Some(src) = &e.src {
            row.insert("src".into(), src.clone().into());
        }
        if e.is_entry {
            row.insert("isEntry".into(), true.into());
        }
        if e.is_dynamic_entry {
            row.insert("isDynamicEntry".into(), true.into());
        }
        let imports = remap(&e.imports);
        if !imports.is_empty() {
            row.insert("imports".into(), imports.into());
        }
        let dyn_imports = remap(&e.dynamic_imports);
        if !dyn_imports.is_empty() {
            row.insert("dynamicImports".into(), dyn_imports.into());
        }
        if !e.css.is_empty() {
            row.insert("css".into(), e.css.clone().into());
        }
        map.insert(e.key.clone(), serde_json::Value::Object(row));
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("oj-{name}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn out_dir_emptiable_follows_vite_and_never_counts_root_as_inside() {
        let root = scratch("emptiable-root");
        let outside = scratch("emptiable-outside");
        let inside = root.join("dist");
        fs::create_dir_all(&inside).unwrap();
        // Vite's default: inside root is emptied ...
        assert!(out_dir_emptiable(&root, &inside, None, false));
        // ... an outDir equal to root is NOT inside (startsWith(root + "/")) ...
        assert!(!out_dir_emptiable(&root, &root, None, false));
        // ... and an outside one is left alone.
        assert!(!out_dir_emptiable(&root, &outside, None, false));
        // An explicit setting wins either way.
        assert!(out_dir_emptiable(&root, &outside, Some(true), false));
        assert!(!out_dir_emptiable(&root, &inside, Some(false), false));
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn prepare_out_dir_refuses_to_empty_the_project_root_but_still_empties_dist() {
        let root = scratch("prepare-root");
        fs::write(root.join("index.html"), "<html></html>").unwrap();
        fs::write(root.join("main.js"), "export {};").unwrap();

        // outDir == root with an explicit emptyOutDir would delete the sources the
        // build reads; refuse instead of destroying the project.
        let err = prepare_out_dir(&root, &root, Some(true)).unwrap_err().to_string();
        assert!(err.contains("project root"), "{err}");
        assert!(root.join("index.html").exists() && root.join("main.js").exists());

        // With Vite's default rule an equal path is simply not emptied.
        prepare_out_dir(&root, &root, None).unwrap();
        assert!(root.join("index.html").exists() && root.join("main.js").exists());

        // A real inside outDir is still emptied, keeping only .git.
        let dist = root.join("dist");
        fs::create_dir_all(dist.join(".git")).unwrap();
        fs::write(dist.join("stale.js"), "old").unwrap();
        prepare_out_dir(&root, &dist, None).unwrap();
        assert!(!dist.join("stale.js").exists());
        assert!(dist.join(".git").is_dir());
        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn module_preload_configuration_controls_production_links() {
        for (index, (config_name, config, expected)) in [
            (
                "oj.config.json",
                r#"{"build":{"modulePreload":false}}"#,
                false,
            ),
            (
                "vite.config.mjs",
                "export default { build: { modulePreload: false } };",
                false,
            ),
            (
                "oj.config.json",
                r#"{"build":{"modulePreload":{"polyfill":false}}}"#,
                true,
            ),
            ("oj.config.json", "{}", true),
        ]
        .into_iter()
        .enumerate()
        {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("oj-module-preload-{suffix}-{index}"));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
            fs::write(root.join(config_name), config).unwrap();
            fs::write(
                root.join("index.html"),
                r#"<html><head></head><body>
                    <script type="module" src="/first.js"></script>
                    <script type="module" src="/second.js"></script>
                </body></html>"#,
            )
            .unwrap();
            fs::write(
                root.join("first.js"),
                r#"import { shared } from "./shared.js"; window.first = shared;"#,
            )
            .unwrap();
            fs::write(
                root.join("second.js"),
                r#"import { shared } from "./shared.js"; window.second = shared;"#,
            )
            .unwrap();
            fs::write(root.join("shared.js"), r#"export const shared = "ready";"#).unwrap();

            build(root.clone(), None, None, Some("production"), false)
                .await
                .expect("synthetic shared-chunk fixture should build");

            let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
            assert_eq!(
                html.contains("rel=\"modulepreload\""),
                expected,
                "{config_name}: {config}\n{html}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn module_type_for_id_infers_from_extension() {
        use rolldown_common::ModuleType;
        // Vite derives the transform lang from the id's extension (query
        // stripped); oj mirrors it for plugin-loaded/virtual modules.
        assert!(matches!(module_type_for_id("~icons/ph/x.jsx"), ModuleType::Jsx));
        assert!(matches!(module_type_for_id("/v/comp.tsx?used"), ModuleType::Tsx));
        assert!(matches!(module_type_for_id("virtual:mod.ts"), ModuleType::Ts));
        assert!(matches!(module_type_for_id("a.mts"), ModuleType::Ts));
        assert!(matches!(module_type_for_id("a.cts"), ModuleType::Ts));
        assert!(matches!(module_type_for_id("data.json#x"), ModuleType::Json));
        assert!(matches!(module_type_for_id("mod.mjs"), ModuleType::Js));
        assert!(matches!(module_type_for_id("plain.js"), ModuleType::Js));
        // Extensionless virtual ids default to JavaScript.
        assert!(matches!(module_type_for_id("\0virtual:store"), ModuleType::Js));
    }

    #[test]
    fn normalize_input_entries_covers_string_array_object() {
        assert_eq!(
            normalize_input_entries(&serde_json::json!("index.html")),
            vec![("index".to_string(), "index.html".to_string())]
        );
        assert_eq!(
            normalize_input_entries(&serde_json::json!("src/pages/options/index.html")),
            vec![(
                "index".to_string(),
                "src/pages/options/index.html".to_string()
            )]
        );
        assert_eq!(
            normalize_input_entries(&serde_json::json!(["a.html", "b.html"])),
            vec![
                ("a".to_string(), "a.html".to_string()),
                ("b".to_string(), "b.html".to_string()),
            ]
        );
        let obj = normalize_input_entries(&serde_json::json!({
            "admin": "src/admin.html",
            "app": "src/app.html",
        }));
        assert!(obj.contains(&("admin".to_string(), "src/admin.html".to_string())));
        assert!(obj.contains(&("app".to_string(), "src/app.html".to_string())));
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn resolve_html_ref_is_relative_to_the_page_or_root() {
        let root = Path::new("/proj");
        let page_dir = Path::new("/proj/src/pages/options");
        // Root-absolute refs resolve against the project root.
        assert_eq!(
            resolve_html_ref("/src/main.tsx", page_dir, root),
            PathBuf::from("/proj/src/main.tsx")
        );
        // Relative refs resolve against the HTML file's own directory.
        assert_eq!(
            resolve_html_ref("./index.tsx", page_dir, root),
            PathBuf::from("/proj/src/pages/options/index.tsx")
        );
        // `..` is resolved lexically so the path matches rolldown's facade id.
        assert_eq!(
            resolve_html_ref("../../main.tsx", page_dir, root),
            PathBuf::from("/proj/src/main.tsx")
        );
        assert_eq!(
            resolve_html_ref("main.tsx", page_dir, root),
            PathBuf::from("/proj/src/pages/options/main.tsx")
        );
    }

    #[test]
    fn insert_before_head_places_snippet_or_prepends() {
        assert_eq!(
            insert_before_head("<head></head><body></body>", "<x>"),
            "<head><x>\n</head><body></body>"
        );
        assert_eq!(insert_before_head("<body></body>", "<x>"), "<x>\n<body></body>");
    }

    #[tokio::test]
    async fn copy_public_dir_configuration_controls_production_assets() {
        for (index, (config_name, config, copied)) in [
            (
                "oj.config.json",
                r#"{"build":{"copyPublicDir":false}}"#,
                false,
            ),
            (
                "vite.config.mjs",
                "export default { build: { copyPublicDir: false } };",
                false,
            ),
            (
                "oj.config.json",
                r#"{"build":{"copyPublicDir":true}}"#,
                true,
            ),
            ("oj.config.json", "{}", true),
        ]
        .into_iter()
        .enumerate()
        {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("oj-copy-public-dir-{suffix}-{index}"));
            fs::create_dir_all(root.join("public")).unwrap();
            fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
            fs::write(root.join(config_name), config).unwrap();
            fs::write(
                root.join("index.html"),
                r#"<html><body><script type="module" src="/main.js"></script></body></html>"#,
            )
            .unwrap();
            fs::write(root.join("main.js"), "window.ready = true;").unwrap();
            fs::write(root.join("public/asset.txt"), "public asset").unwrap();

            build(root.clone(), None, None, Some("production"), false)
                .await
                .expect("synthetic public-directory fixture should build");

            assert_eq!(
                root.join("dist/asset.txt").is_file(),
                copied,
                "{config_name}: {config}"
            );
            assert!(root.join("dist/index.html").is_file());
            fs::remove_dir_all(root).unwrap();
        }
    }

    /// Vite's define plugin: a browser bundle gets `process.env` -> `{}` (so an
    /// arbitrary `process.env.X` is `undefined`, not a ReferenceError) and
    /// NODE_ENV inlined; a server bundle keeps `process.env` (keepProcessEnv).
    #[tokio::test]
    async fn client_build_defines_process_env_to_an_empty_object_and_ssr_keeps_it() {
        let root = scratch("process-env-define");
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("index.html"),
            r#"<html><body><script type="module" src="/main.js"></script></body></html>"#,
        )
        .unwrap();
        fs::write(
            root.join("main.js"),
            "export const probe = [process.env.SOME_FLAG, globalThis.process.env.OTHER, process.env.NODE_ENV];\n\
             window.probe = probe;\n",
        )
        .unwrap();
        fs::write(root.join("server.js"), "export const flag = process.env.SOME_FLAG;\n").unwrap();

        build(root.clone(), None, None, Some("production"), false)
            .await
            .expect("client build");
        let mut client = String::new();
        for entry in fs::read_dir(root.join("dist/assets")).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "js") {
                client.push_str(&fs::read_to_string(p).unwrap());
            }
        }
        assert!(!client.contains("process.env"), "client bundle still reads process.env:\n{client}");
        assert!(client.contains("\"production\"") || client.contains("`production`"));

        build_ssr(&root, &root.join("dist-ssr"), "server.js", "production", oj_config::Sourcemap::Off)
            .await
            .expect("ssr build");
        let server = fs::read_to_string(root.join("dist-ssr/server.mjs")).unwrap();
        assert!(server.contains("process.env.SOME_FLAG"), "ssr bundle must keep process.env:\n{server}");
        fs::remove_dir_all(root).unwrap();
    }

    /// Vite's build-html asset pass: attribute references to source assets are
    /// emitted hashed (or inlined under assetsInlineLimit, never for a link
    /// icon/manifest or a fragment svg), publicDir references get the base
    /// prefix, srcset candidates are handled one by one, and external,
    /// missing and `vite-ignore` references stay as written.
    #[tokio::test]
    async fn html_asset_attributes_are_rewritten_like_vite() {
        let root = scratch("html-asset-attrs");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("public")).unwrap();
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(root.join("vite.config.mjs"), "export default { base: '/app/' };").unwrap();
        fs::write(root.join("src/big.png"), vec![7u8; 9000]).unwrap();
        fs::write(root.join("src/big-2x.png"), vec![8u8; 9000]).unwrap();
        fs::write(root.join("src/tiny.png"), vec![9u8; 40]).unwrap();
        fs::write(root.join("src/icon.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"r\"/></svg>").unwrap();
        fs::write(root.join("public/logo.png"), vec![1u8; 100]).unwrap();
        fs::write(root.join("src/main.js"), "console.log(1);").unwrap();
        fs::write(
            root.join("index.html"),
            "<html><head>\n\
             <link rel=\"icon\" href=\"./src/icon.svg\">\n\
             <link rel=\"manifest\" href=\"/manifest.webmanifest\">\n\
             <meta property=\"og:image\" content=\"/src/big.png\">\n\
             <meta name=\"description\" content=\"/src/big.png\">\n\
             </head><body>\n\
             <img src=\"./src/big.png\" alt=\"a\">\n\
             <img src='src/tiny.png' srcset=\"./src/big.png 1x, ./src/big-2x.png 2x\">\n\
             <img src=\"/logo.png\">\n\
             <img src=\"https://example.com/x.png\">\n\
             <img vite-ignore src=\"./src/big.png\">\n\
             <video poster=\"src/big.png\" src=\"/missing.mp4\"></video>\n\
             <svg><use href=\"./src/icon.svg#r\"></use></svg>\n\
             <script type=\"module\" src=\"/src/main.js\"></script></body></html>",
        )
        .unwrap();
        fs::write(root.join("public/manifest.webmanifest"), "{}").unwrap();

        build(root.clone(), None, None, Some("production"), false)
            .await
            .expect("html asset fixture should build");
        let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
        let big = fs::read_dir(root.join("dist/assets"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with("big-") && !n.starts_with("big-2x") && n.ends_with(".png"))
            .expect("big.png emitted hashed");
        let big_url = format!("/app/assets/{big}");
        assert!(html.contains(&format!("<img src=\"{big_url}\" alt=\"a\">")), "{html}");
        assert!(html.contains(&format!("<meta property=\"og:image\" content=\"{big_url}\">")), "{html}");
        assert!(html.contains("<meta name=\"description\" content=\"/src/big.png\">"), "{html}");
        assert!(html.contains(&format!("<video poster=\"{big_url}\" src=\"/missing.mp4\">")), "{html}");
        assert!(html.contains(&format!("srcset=\"{big_url} 1x, /app/assets/big-2x-")), "{html}");
        // small: inlined, keeping the page's single quotes
        assert!(html.contains("<img src='data:image/png;base64,"), "{html}");
        // link icon is never inlined, and its fragment use shares the same file
        assert!(html.contains("<link rel=\"icon\" href=\"/app/assets/icon-"), "{html}");
        assert!(html.contains("<use href=\"/app/assets/icon-") && html.contains(".svg#r\">"), "{html}");
        // publicDir references get the base prefix
        assert!(html.contains("<link rel=\"manifest\" href=\"/app/manifest.webmanifest\">"), "{html}");
        assert!(html.contains("<img src=\"/app/logo.png\">"), "{html}");
        // left alone: external, vite-ignore (attribute dropped), missing file
        assert!(html.contains("<img src=\"https://example.com/x.png\">"), "{html}");
        assert!(html.contains("<img src=\"./src/big.png\">"), "{html}");
        assert!(!html.contains("vite-ignore"), "{html}");
        fs::remove_dir_all(root).unwrap();
    }

    /// `build.lib` from vite.config: Vite's entry forms, default formats
    /// (es+umd for one entry, es+cjs for several), file names from the package
    /// name and `type`, and `cssFileName`.
    #[tokio::test]
    async fn vite_config_build_lib_is_honored_with_vite_naming() {
        // One string entry in a `type: module` package named `@scope/my-lib`.
        let root = scratch("vite-lib-single");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"@scope/my-lib","type":"module"}"#).unwrap();
        fs::write(root.join("src/index.ts"), "import './style.css';\nexport const add = (a: number, b: number) => a + b;\n").unwrap();
        fs::write(root.join("src/style.css"), ".a{color:red}").unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { build: { lib: { entry: 'src/index.ts', name: 'MyLib' } } };",
        )
        .unwrap();
        build(root.clone(), None, None, Some("production"), false)
            .await
            .expect("vite.config build.lib should build");
        assert!(root.join("dist/my-lib.js").is_file(), "es output named after the unscoped package");
        assert!(root.join("dist/my-lib.umd.cjs").is_file(), "umd is a default format; .cjs under type module");
        assert!(root.join("dist/my-lib.css").is_file());
        assert!(!root.join("dist/index.js").exists());
        fs::remove_dir_all(&root).unwrap();

        // Several aliased entries in a commonjs package: es+cjs, per-entry names.
        let root = scratch("vite-lib-multi");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"multi"}"#).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
        fs::write(root.join("src/b.ts"), "export const b = 2;\n").unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { build: { lib: { entry: { main: 'src/a.ts', extra: 'src/b.ts' }, cssFileName: 'theme' } } };",
        )
        .unwrap();
        build(root.clone(), None, None, Some("production"), false)
            .await
            .expect("multi-entry lib should build");
        for f in ["main.mjs", "extra.mjs", "main.js", "extra.js"] {
            assert!(root.join("dist").join(f).is_file(), "missing {f}");
        }
        assert!(!root.join("dist/main.umd.js").exists(), "umd is not a default with several entries");
        fs::remove_dir_all(&root).unwrap();

        // umd without a name is Vite's error, not a silent es-only build.
        let root = scratch("vite-lib-noname");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"noname"}"#).unwrap();
        fs::write(root.join("src/index.ts"), "export const a = 1;\n").unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { build: { lib: { entry: 'src/index.ts', formats: ['umd'] } } };",
        )
        .unwrap();
        let err = build(root.clone(), None, None, Some("production"), false)
            .await
            .expect_err("umd needs build.lib.name");
        assert!(err.to_string().contains("build.lib.name"), "{err}");
        fs::remove_dir_all(&root).unwrap();
    }

    /// Vite's `bundleWorkerEntry`: a `?worker&inline` bundle is built under the
    /// app config, so an aliased import, `define` and the client `process.env`
    /// defines resolve the same way inside the worker as in the main bundle.
    #[tokio::test]
    async fn inline_worker_is_bundled_with_the_app_config() {
        let root = scratch("worker-inline-config");
        fs::create_dir_all(root.join("src/lib")).unwrap();
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "import path from 'node:path';\n\
             export default { resolve: { alias: { '@': path.resolve(import.meta.dirname, 'src') } }, define: { __APP_VERSION__: '\"1.2.3\"' } };\n",
        )
        .unwrap();
        fs::write(root.join("src/lib/util.js"), "export const greet = (n) => 'hi ' + n;\n").unwrap();
        fs::write(
            root.join("src/w.js"),
            "import { greet } from '@/lib/util';\n\
             self.postMessage(greet(__APP_VERSION__) + ':' + process.env.NODE_ENV + ':' + String(process.env.NOPE));\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main.js"),
            "import W from './w.js?worker&inline';\nnew W().onmessage = (e) => { document.body.textContent = e.data; };\n",
        )
        .unwrap();
        fs::write(
            root.join("index.html"),
            r#"<html><body><script type="module" src="/src/main.js"></script></body></html>"#,
        )
        .unwrap();
        build(root.clone(), None, None, Some("production"), false)
            .await
            .expect("inline worker fixture should build");
        let mut code = String::new();
        for entry in fs::read_dir(root.join("dist/assets")).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "js") {
                code.push_str(&fs::read_to_string(p).unwrap());
            }
        }
        assert!(!code.contains("@/lib"), "alias not resolved inside the inline worker:\n{code}");
        assert!(!code.contains("__APP_VERSION__"), "define not applied inside the inline worker:\n{code}");
        assert!(code.contains("1.2.3") && code.contains("hi "), "{code}");
        assert!(!code.contains("process.env"), "process.env defines missing inside the inline worker:\n{code}");
        fs::remove_dir_all(root).unwrap();
    }

    /// The plugin host of an SSR (and client-entry) build is told the real mode,
    /// so a plugin's `configResolved(config).mode` under `--mode staging` is
    /// "staging" as in Vite, not a hardcoded "production".
    #[tokio::test]
    async fn ssr_build_plugins_see_the_real_mode() {
        let root = scratch("ssr-plugin-mode");
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "import fs from 'node:fs';\n\
             export default { plugins: [{ name: 'mode-probe', configResolved(c) { fs.writeFileSync(new URL('./mode.txt', import.meta.url), c.mode); } }] };\n",
        )
        .unwrap();
        fs::write(root.join("server.js"), "export const render = () => 'ok';\n").unwrap();
        build_ssr(&root, &root.join("dist-ssr"), "server.js", "staging", oj_config::Sourcemap::Off)
            .await
            .expect("ssr build");
        let seen = fs::read_to_string(root.join("mode.txt")).expect("plugin configResolved ran");
        assert_eq!(seen.trim(), "staging");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manifest_matches_vite_shape() {
        let mk = |key: &str,
                  name: &str,
                  file: &str,
                  src: Option<&str>,
                  is_entry: bool,
                  is_dyn: bool,
                  imports: Vec<&str>,
                  dyn_imports: Vec<&str>,
                  css: Vec<&str>| ManifestEntry {
            key: key.into(),
            name: name.into(),
            file: file.into(),
            src: src.map(str::to_string),
            is_entry,
            is_dynamic_entry: is_dyn,
            imports: imports.into_iter().map(str::to_string).collect(),
            dynamic_imports: dyn_imports.into_iter().map(str::to_string).collect(),
            css: css.into_iter().map(str::to_string).collect(),
        };
        let m = build_manifest(&[
            mk(
                "src/main.tsx",
                "main",
                "assets/main-abc123.js",
                Some("src/main.tsx"),
                true,
                false,
                vec!["assets/vendor-def456.js"],
                vec!["assets/lazy-xyz.js"],
                vec!["assets/style-99.css"],
            ),
            mk(
                "_vendor-def456.js",
                "vendor",
                "assets/vendor-def456.js",
                None,
                false,
                false,
                vec![],
                vec![],
                vec![],
            ),
            mk(
                "_lazy-xyz.js",
                "lazy",
                "assets/lazy-xyz.js",
                None,
                false,
                true,
                vec![],
                vec![],
                vec![],
            ),
        ]);
        let row = &m["src/main.tsx"];
        assert_eq!(row["file"], "assets/main-abc123.js");
        assert_eq!(row["name"], "main");
        assert_eq!(row["src"], "src/main.tsx");
        assert_eq!(row["isEntry"], true);
        // imports/dynamicImports reference manifest KEYS, not output filenames.
        assert_eq!(row["imports"][0], "_vendor-def456.js");
        assert_eq!(row["dynamicImports"][0], "_lazy-xyz.js");
        assert_eq!(row["css"][0], "assets/style-99.css");
        // Non-entry chunk is present, keyed by _<filename>, with no src.
        assert_eq!(m["_vendor-def456.js"]["file"], "assets/vendor-def456.js");
        assert!(m["_vendor-def456.js"].get("src").is_none());
        // Dynamic entry is flagged.
        assert_eq!(m["_lazy-xyz.js"]["isDynamicEntry"], true);
    }

    #[test]
    fn non_entry_omits_isentry_and_empty_fields() {
        let m = build_manifest(&[ManifestEntry {
            key: "_chunk-1.js".into(),
            name: "chunk".into(),
            file: "assets/chunk-1.js".into(),
            src: None,
            is_entry: false,
            is_dynamic_entry: false,
            imports: vec![],
            dynamic_imports: vec![],
            css: vec![],
        }]);
        let row = m["_chunk-1.js"].as_object().unwrap();
        assert!(!row.contains_key("isEntry"));
        assert!(!row.contains_key("isDynamicEntry"));
        assert!(!row.contains_key("imports"));
        assert!(!row.contains_key("dynamicImports"));
        assert!(!row.contains_key("css"));
        assert!(!row.contains_key("src"));
    }

    #[test]
    fn lib_format_mapping() {
        assert_eq!(lib_format("es"), Some(false));
        assert_eq!(lib_format("esm"), Some(false));
        assert_eq!(lib_format("cjs"), Some(false));
        assert_eq!(lib_format("umd"), Some(true));
        assert_eq!(lib_format("iife"), Some(true));
        assert_eq!(lib_format("amd"), None);
    }

    #[test]
    fn base_normalization_and_application() {
        assert_eq!(normalize_base("/"), "/");
        assert_eq!(normalize_base(""), "./");
        assert_eq!(normalize_base("./"), "./");
        assert_eq!(normalize_base("app"), "/app/");
        assert_eq!(normalize_base("/app"), "/app/");
        assert_eq!(normalize_base("/app/"), "/app/");
        assert_eq!(with_base("assets/x-h.js", "/"), "/assets/x-h.js");
        assert_eq!(with_base("assets/x-h.js", "/app/"), "/app/assets/x-h.js");
        assert_eq!(with_base("assets/x-h.js", "./"), "./assets/x-h.js");
    }

    #[test]
    fn relative_base_is_computed_per_page_stylesheet_and_chunk() {
        assert_eq!(page_base("/app/", "nested/index.html"), "/app/");
        assert_eq!(page_base("./", "index.html"), "./");
        assert_eq!(page_base("./", "nested/index.html"), "../");
        assert_eq!(page_base("./", "a/b/index.html"), "../../");
        assert_eq!(css_asset_url("assets/img-h.png", "./"), "./img-h.png");
        assert_eq!(css_asset_url("assets/img-h.png", "/app/"), "/app/assets/img-h.png");
        assert_eq!(relative_chunk_path("assets/main-h.js", "assets/lazy-h.js"), "./lazy-h.js");
        assert_eq!(relative_chunk_path("main-h.js", "assets/lazy-h.js"), "./assets/lazy-h.js");
        assert_eq!(relative_chunk_path("assets/a/main-h.js", "assets/lazy-h.js"), "../lazy-h.js");
    }

    #[test]
    fn module_script_srcs_only_module_type_absolute() {
        let html = r#"<script type="module" src="/src/main.tsx"></script>
                      <script src="/legacy.js"></script>
                      <script type="module" src="https://cdn/x.js"></script>"#;
        let srcs = module_script_srcs(html);
        assert_eq!(srcs, vec!["/src/main.tsx"]);
    }

    #[test]
    fn module_script_srcs_accepts_relative_entries() {
        let html = r#"<script type="module" src="src/index.tsx"></script>"#;
        assert_eq!(module_script_srcs(html), vec!["src/index.tsx"]);
        let html2 = r#"<script type="module" src="./app/main.ts"></script>"#;
        assert_eq!(module_script_srcs(html2), vec!["./app/main.ts"]);
    }

    #[test]
    fn is_server_module_path_matches_server_suffixes() {
        for yes in [
            "api.server.ts",
            "a/b/auth.server.tsx",
            "x.server.js",
            "y.server.jsx",
        ] {
            assert!(
                is_server_module_path(yes),
                "{yes} should be a server module"
            );
        }
        for no in [
            "api.ts",
            "server.ts",
            "api.server.css",
            "a.serverx.ts",
            "note.server.md",
        ] {
            assert!(
                !is_server_module_path(no),
                "{no} should not be a server module"
            );
        }
    }

    #[test]
    fn server_fn_prod_stub_emits_an_rpc_per_export() {
        let out = server_fn_prod_stub(&["getUser".into(), "default".into()], "/api.server.ts");
        assert!(
            out.contains("const __ojCall ="),
            "the fetch helper is inlined: {out}"
        );
        assert!(
            out.contains(
                r#"export const getUser = (...a) => __ojCall("/api.server.ts", "getUser", a);"#
            ),
            "named export stub: {out}"
        );
        assert!(
            out.contains(r#"export default (...a) => __ojCall("/api.server.ts", "default", a);"#),
            "default export stub: {out}"
        );
        let empty = server_fn_prod_stub(&[], "/x.server.ts");
        assert!(empty.contains("__ojCall"));
        assert!(
            !empty.contains("export "),
            "no exports means no stubs: {empty}"
        );
    }

    /// Whatever an inlined asset's bytes are, the module oj emits for it has to
    /// parse. The url is derived from a file oj did not write.
    fn parses(code: &str) -> bool {
        oj_compiler::compile(
            std::path::Path::new("/verify.mjs"),
            code,
            &oj_compiler::CompileOptions::prod(),
        )
        .is_ok()
    }

    #[test]
    fn an_inlined_svg_with_crlf_endings_emits_parseable_javascript() {
        // git hands a Windows checkout CRLF, and a raw carriage return cannot
        // appear in a JavaScript string literal.
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\">\r\n  <rect/>\r\n</svg>\r\n";
        let url = svg_data_url(svg);
        assert!(!url.contains('\r'), "carriage return survived: {url}");
        assert!(!url.contains('\n'), "line feed survived: {url}");
        assert!(url.starts_with("data:image/svg+xml,"), "{url}");
        assert!(url.contains("%3Crect/%3E"), "content lost: {url}");
        assert!(parses(&export_default_url(&url)), "{url}");
    }

    #[test]
    fn svg_data_urls_encode_everything_that_would_break_the_url_or_the_literal() {
        let cases = [
            ("<svg/>", "%3Csvg/%3E"),
            ("<svg a=\"b\"/>", "%3Csvg a='b'/%3E"),
            ("<svg>100%</svg>", "%3Csvg%3E100%25%3C/svg%3E"),
            ("<svg fill=\"#fff\"/>", "%3Csvg fill='%23fff'/%3E"),
            ("<svg>a\\b</svg>", "%3Csvg%3Ea%5Cb%3C/svg%3E"),
        ];
        for (svg, expected_tail) in cases {
            let url = svg_data_url(svg);
            assert_eq!(url, format!("data:image/svg+xml,{expected_tail}"));
            assert!(parses(&export_default_url(&url)), "{url}");
        }
        // Line and paragraph separators are string-literal terminators too.
        for terminator in ["\u{2028}", "\u{2029}", "\r", "\n", "\r\n"] {
            let url = svg_data_url(&format!("<svg>{terminator}</svg>"));
            assert_eq!(url, "data:image/svg+xml,%3Csvg%3E%3C/svg%3E");
            assert!(parses(&export_default_url(&url)));
        }
    }

    #[test]
    fn a_default_export_of_a_url_is_always_a_valid_module() {
        for url in [
            "data:image/png;base64,AAAA",
            "/assets/a-0123.png",
            "",
            "a\"b",
            "a\\b",
            "a\nb",
            "a\rb",
            "a\u{2028}b",
            "a\0b",
            "café/🚀.png",
        ] {
            let module = export_default_url(url);
            assert!(parses(&module), "{url:?} -> {module}");
        }
    }

    #[test]
    fn every_asset_extension_has_a_real_mime_type() {
        // `is_build_asset` decides what gets inlined; `asset_mime` decides what
        // the browser is told it is. A gap between them inlines a file the page
        // then refuses to render.
        for ext in [
            "png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "bmp", "svg", "woff", "woff2",
            "ttf", "otf", "eot", "mp4", "webm", "mov", "mp3", "wav", "ogg",
        ] {
            assert!(is_build_asset(&format!("/src/a.{ext}")), "{ext} not an asset");
            assert_ne!(
                asset_mime(ext),
                "application/octet-stream",
                "{ext} has no mime type"
            );
        }
        // A query does not change the classification.
        assert!(is_build_asset("/src/a.png?url"));
        // Same list as the dev server, case-insensitive like Vite.
        assert!(is_build_asset("/src/photo.JPG"));
        assert!(is_build_asset("/src/doc.pdf"));
        assert!(is_build_asset("/src/site.webmanifest"));
        assert_eq!(asset_mime("PNG"), "image/png");
        assert!(!is_build_asset("/src/a.tsx"));
        assert!(!is_build_asset("/src/a.png.tsx"));
        assert!(!is_build_asset(""));
        // An unknown extension falls back rather than guessing.
        assert_eq!(asset_mime("xyz"), "application/octet-stream");
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 section 10, plus the padding boundaries.
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        // High bytes must not sign-extend.
        assert_eq!(b64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(b64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(b64(&[0xfb, 0xff, 0xbf]), "+/+/");
        // Length is always a multiple of four.
        for len in 0..64 {
            let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(b64(&bytes).len() % 4, 0, "len {len}");
        }
    }

    #[test]
    fn asset_queries_split_in_any_combination() {
        assert_eq!(split_asset_query("./w.ts?worker"), Some(("./w.ts".into(), "worker".into())));
        assert_eq!(split_asset_query("./w.ts?worker&inline"), Some(("./w.ts".into(), "worker&inline".into())));
        assert_eq!(split_asset_query("./w.ts?inline&worker"), Some(("./w.ts".into(), "worker&inline".into())));
        assert_eq!(split_asset_query("./w.ts?url&sharedworker"), Some(("./w.ts".into(), "sharedworker&url".into())));
        assert_eq!(split_asset_query("./a.png?url"), Some(("./a.png".into(), "url".into())));
        assert_eq!(split_asset_query("./a.png?url&v=1"), None, "foreign params are not oj queries");
        assert_eq!(split_asset_query("./a.png?v=1"), None);
        assert_eq!(split_asset_query("./a.png"), None);
        assert_eq!(worker_id_parts("/x/w.ts?worker&inline"), Some(("/x/w.ts", "Worker", true, false)));
        assert_eq!(worker_id_parts("/x/w.ts?sharedworker&url"), Some(("/x/w.ts", "SharedWorker", false, true)));
        assert_eq!(worker_id_parts("/x/w.ts?url"), None);
    }

    #[test]
    fn asset_file_names_render_vite_placeholders() {
        assert_eq!(render_asset_name(None, "bg", "0123456789abcdef", "png"), "assets/bg-01234567.png");
        assert_eq!(render_asset_name(Some("static/[name].[hash][extname]"), "bg", "0123456789abcdef", "png"), "static/bg.01234567.png");
        assert_eq!(render_asset_name(Some("[ext]/[name]-[hash].[ext]"), "app", "abcdef0123456789", "css"), "css/app-abcdef01.css");
        assert_eq!(render_asset_name(None, "x", "abcd", ""), "assets/x-abcd");
    }

    #[test]
    fn content_hashes_are_stable_and_content_addressed() {
        // Pinned: filenames derived from this end up in caching headers, so the
        // value must not drift with the toolchain that built oj.
        assert_eq!(content_hash(b""), "af1349b9f5f9a1a6");
        assert_eq!(content_hash(b"oj"), "b74f11b8dbbdd6b4");
        assert_eq!(content_hash(b"oj").len(), 16);
        assert!(content_hash(b"oj").chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(content_hash(b"a"), content_hash(b"b"));
        // A one-bit difference is a different name.
        assert_ne!(content_hash(&[0x00]), content_hash(&[0x01]));
    }

    #[test]
    fn transitive_imports_terminates_on_a_cycle() {
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), vec!["b".to_string()]);
        map.insert("b".to_string(), vec!["c".to_string(), "a".to_string()]);
        map.insert("c".to_string(), vec!["b".to_string()]);
        let reachable = transitive_imports("a", &map);
        assert_eq!(
            reachable.into_iter().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // An unknown entry reaches nothing.
        assert!(transitive_imports("missing", &map).is_empty());
        // A self-import terminates.
        let mut selfish = std::collections::HashMap::new();
        selfish.insert("a".to_string(), vec!["a".to_string()]);
        assert_eq!(
            transitive_imports("a", &selfish).into_iter().collect::<Vec<_>>(),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn asset_names_are_sanitized_without_collapsing_to_nothing() {
        assert_eq!(sanitize_asset_name("logo.png"), "logo.png");
        assert_eq!(sanitize_asset_name("my logo.png"), "my_logo.png");
        assert_eq!(sanitize_asset_name("a/b.png"), "a_b.png");
        assert_eq!(sanitize_asset_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_asset_name("a\0b.png"), "a_b.png");
        assert!(!sanitize_asset_name("café.png").contains('é'));
        assert!(sanitize_asset_name("").is_empty());
    }

    #[test]
    fn human_bytes_scales_by_threshold() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.0kB");
        assert_eq!(human_bytes(1536), "1.5kB");
        assert_eq!(human_bytes(1_048_575), "1024.0kB");
        assert_eq!(human_bytes(1_048_576), "1.0MB");
        assert_eq!(human_bytes(3_145_728), "3.0MB");
    }

    #[test]
    fn rewrite_link_hrefs_keeps_the_pages_quoting() {
        let html = "<link rel='stylesheet' href='/site.css'><link rel=stylesheet href=/site.css><link rel=\"stylesheet\" href = \"/site.css\"><link href=\"/other.css\">";
        let out = rewrite_link_hrefs(html, "/site.css", "/assets/site-abc.css");
        assert_eq!(
            out,
            "<link rel='stylesheet' href='/assets/site-abc.css'><link rel=stylesheet href=/assets/site-abc.css><link rel=\"stylesheet\" href = \"/assets/site-abc.css\"><link href=\"/other.css\">"
        );
        assert_eq!(rewrite_link_hrefs("<p>no links</p>", "/a", "/b"), "<p>no links</p>");
    }

    #[test]
    fn stylesheet_hrefs_take_local_stylesheet_links_only() {
        let html = r#"<html><head>
          <link rel="stylesheet" href="/src/base.css">
          <link rel="stylesheet" href="./src/theme.scss">
          <link rel="stylesheet" href="https://cdn.test/x.css">
          <link rel="icon" href="favicon.ico">
          <link rel="modulepreload" href="/assets/chunk.js">
        </head></html>"#;
        assert_eq!(stylesheet_hrefs(html), vec!["/src/base.css".to_string(), "./src/theme.scss".to_string()]);
    }

    #[test]
    fn inline_module_scripts_become_html_proxy_entries() {
        let html = "<html><body>\n<script type=\"module\">import { x } from \"./src/x.js\"; x();</script>\n<script type=\"module\" src=\"/src/main.js\"></script>\n<script>var legacy = 1;</script>\n<script type=\"module\">console.log(2)</script></body></html>";
        let blocks = inline_module_scripts(html);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].2.contains("import { x }"));
        let (out, entries) = externalize_inline_scripts(html, Path::new("/app/index.html"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "/@oj-inline/0.js");
        assert_eq!(entries[0].1, "/app/index.html?html-proxy&index=0.js");
        assert_eq!(entries[1].1, "/app/index.html?html-proxy&index=1.js");
        assert!(out.contains("<script type=\"module\" src=\"/@oj-inline/0.js\"></script>"), "{out}");
        assert!(out.contains("<script type=\"module\" src=\"/@oj-inline/1.js\"></script>"), "{out}");
        assert!(!out.contains("console.log(2)"), "inline body removed: {out}");
        assert!(out.contains("var legacy = 1;"), "classic scripts untouched: {out}");
        assert!(out.contains("src=\"/src/main.js\""), "external module kept: {out}");
        // The placeholder is picked up as a module entry src.
        assert!(module_script_srcs(&out).contains(&"/@oj-inline/0.js".to_string()));
    }

}
