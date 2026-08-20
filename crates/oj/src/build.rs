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

fn ro_output_str(ro: Option<&serde_json::Value>, key: &str) -> Option<String> {
    let output = ro?.get("output")?;
    let obj = if output.is_array() {
        output.get(0)?
    } else {
        output
    };
    obj.get(key)?.as_str().map(String::from)
}

fn ro_external(ro: Option<&serde_json::Value>) -> Vec<String> {
    match ro.and_then(|v| v.get("external")) {
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

fn manual_chunks(ro: Option<&serde_json::Value>) -> Option<rolldown_common::CodeSplittingMode> {
    let output = ro?.get("output")?;
    let output = if output.is_array() {
        output.get(0)?
    } else {
        output
    };
    let map = output.get("manualChunks")?.as_object()?;
    let mut groups = Vec::new();
    let mut priority = map.len() as u32;
    for (name, tokens) in map {
        let Some(arr) = tokens.as_array() else {
            continue;
        };
        let escaped: Vec<String> = arr
            .iter()
            .filter_map(|t| t.as_str())
            .map(re_escape)
            .collect();
        if escaped.is_empty() {
            continue;
        }
        let pattern = format!(r"[\\/]node_modules[\\/]({})([\\/]|$)", escaped.join("|"));
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

fn target_transform(
    config: &oj_config::OjConfig,
) -> Option<rolldown_common::BundlerTransformOptions> {
    let target = config.build.as_ref().and_then(|b| b.target.clone())?;
    Some(rolldown_common::BundlerTransformOptions {
        target: Some(rolldown_common::Either::Left(target)),
        ..Default::default()
    })
}

fn is_build_asset(id: &str) -> bool {
    matches!(
        std::path::Path::new(id.split('?').next().unwrap_or(id))
            .extension()
            .and_then(|e| e.to_str()),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "avif"
                | "ico"
                | "bmp"
                | "svg"
                | "woff"
                | "woff2"
                | "ttf"
                | "otf"
                | "eot"
                | "mp4"
                | "webm"
                | "mov"
                | "mp3"
                | "wav"
                | "ogg"
        )
    )
}

fn asset_mime(ext: &str) -> &'static str {
    match ext {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "bmp" => "image/bmp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
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

/// A module that default-exports `url`. The literal is JSON-encoded rather than
/// interpolated: a url is derived from a file oj did not write, and a raw
/// carriage return or quote in it would emit JavaScript that does not parse.
fn export_default_url(url: &str) -> String {
    format!(
        "export default {};",
        serde_json::Value::String(url.to_string())
    )
}

/// An unencoded `data:image/svg+xml,` url, which keeps an inlined SVG readable
/// and smaller than base64 would.
///
/// Percent-encodes what cannot appear unencoded in a data url, and drops the
/// line terminators: a CRLF-formatted SVG -- what git hands a Windows checkout
/// -- used to leave the CR in place, and a raw CR cannot appear in a JavaScript
/// string literal at all.
fn svg_data_url(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for ch in text.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\\' => out.push_str("%5C"),
            // A double quote would close the literal; SVG accepts either quote.
            '"' => out.push('\''),
            // Line terminators: not allowed in a string literal, and not
            // meaningful in SVG markup.
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
        let url_base = args.specifier.strip_suffix("?url").map(str::to_string);
        let init_base = args.specifier.strip_suffix("?init").map(str::to_string);
        let raw_base = args.specifier.strip_suffix("?raw").map(str::to_string);
        let inline_base = args.specifier.strip_suffix("?inline").map(str::to_string);
        let react_base = args.specifier.strip_suffix("?react").map(str::to_string);
        let worker_base = args.specifier.strip_suffix("?worker").map(str::to_string);
        let shared_base = args
            .specifier
            .strip_suffix("?sharedworker")
            .map(str::to_string);
        let importer = args.importer.map(str::to_string);
        let ctx = ctx.clone();
        async move {
            if is_routes {
                return Ok(Some(HookResolveIdOutput::from_id(routes_id)));
            }
            for (base, query) in [
                (url_base, "url"),
                (init_base, "init"),
                (raw_base, "raw"),
                (inline_base, "inline"),
                (react_base, "react"),
                (worker_base, "worker"),
                (shared_base, "sharedworker"),
            ] {
                if let Some(base) = base {
                    if let Ok(Ok(resolved)) = ctx.resolve(&base, importer.as_deref(), None).await {
                        let id = format!("{}?{query}", resolved.id.as_str());
                        return Ok(Some(HookResolveIdOutput::from_id(id)));
                    }
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
        async move {
            if let Some(file) = id.strip_suffix("?url") {
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let code = emit_or_inline(&ctx, file, bytes, inline_limit)?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
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
            let worker = id
                .strip_suffix("?worker")
                .map(|f| (f, "Worker"))
                .or_else(|| {
                    id.strip_suffix("?sharedworker")
                        .map(|f| (f, "SharedWorker"))
                });
            if let Some((file, ctor)) = worker {
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
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "export default function () {{ return new {ctor}(import.meta.ROLLUP_FILE_URL_{reference}, {{ type: \"module\" }}); }};"
                    )),
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
            let is_less = oj_server::sidecar::is_less(path);
            let is_stylus = oj_server::sidecar::is_stylus(path);
            if !(path.ends_with(".css") || oj_css::is_sass(path) || is_less || is_stylus) {
                return Ok(None);
            }
            let mut source = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
            if oj_css::is_sass(path) {
                let dir = std::path::Path::new(path).parent();
                source = oj_css::compile_sass(&source, dir).map_err(|e| anyhow::anyhow!(e))?;
            } else if is_less || is_stylus {
                source = preprocess_via_sidecar(&root, std::path::Path::new(path))?;
            }
            if oj_server::sidecar::is_tailwind_css(&source)
                || (self.has_postcss && path.ends_with(".css"))
            {
                source = expand_css_via_sidecar(&root, std::path::Path::new(path))?;
            }
            let css_id = match std::path::Path::new(path).strip_prefix(&root) {
                Ok(rel) => format!("/{}", rel.display()),
                Err(_) => path.to_string(),
            };
            let output =
                oj_css::compile_css(&css_id, &source, true).map_err(|e| anyhow::anyhow!(e))?;
            let js = match &output.exports {
                Some(exports) => {
                    let map: serde_json::Map<String, serde_json::Value> = exports
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                        .collect();
                    format!("export default {};", serde_json::Value::Object(map))
                }
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

fn expand_css_via_sidecar(root: &Path, css_file: &Path) -> anyhow::Result<String> {
    let script = root.join(".oj-cache").join("css-sidecar.mjs");
    if let Some(parent) = script.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&script, oj_server::sidecar::SIDECAR_JS)?;
    let out = std::process::Command::new("node")
        .args([
            script.to_str().unwrap(),
            "--once",
            css_file.to_str().unwrap(),
            root.to_str().unwrap(),
        ])
        .current_dir(root)
        .output()
        .context("node not found for tailwind/postcss build")?;
    if !out.status.success() {
        bail!(
            "css build failed for {}: {}",
            css_file.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn svelte_via_sidecar(root: &Path, file: &Path) -> anyhow::Result<String> {
    use std::io::Write;
    let script = root.join(".oj-cache").join("svelte-compile.mjs");
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

fn preprocess_via_sidecar(root: &Path, css_file: &Path) -> anyhow::Result<String> {
    use std::io::Write;
    let script = root.join(".oj-cache").join("css-preprocess.mjs");
    if let Some(parent) = script.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&script, oj_server::sidecar::PREPROCESS_JS)?;
    let css = fs::read_to_string(css_file)?;
    let req = serde_json::json!({
        "id": 1,
        "base": root.to_string_lossy(),
        "css": css,
        "from": css_file.to_string_lossy(),
    })
    .to_string();
    let mut child = std::process::Command::new("node")
        .arg(&script)
        .current_dir(root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .context("node not found for css preprocess build")?;
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
            "css preprocess failed for {}: {}",
            css_file.display(),
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("is `less`/`stylus` installed?")
        ),
    }
}

fn rolldown_resolve(
    root: &Path,
    config: &oj_config::OjConfig,
    env: &str,
) -> Option<rolldown_common::ResolveOptions> {
    let alias = oj_config::resolve_alias(config, env);
    if alias.is_empty() {
        return None;
    }
    let alias = alias
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
    Some(rolldown_common::ResolveOptions {
        alias: Some(alias),
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

#[derive(Debug)]
struct OjUserPlugin {
    host: Arc<PluginHost>,
    render_chunk_enabled: Arc<tokio::sync::OnceCell<bool>>,
}

impl OjUserPlugin {
    fn new(host: Arc<PluginHost>) -> Self {
        Self {
            host,
            render_chunk_enabled: Arc::new(tokio::sync::OnceCell::new()),
        }
    }
}

impl Plugin for OjUserPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:user-plugins")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
            | rolldown_plugin::HookUsage::Load
            | rolldown_plugin::HookUsage::Transform
            | rolldown_plugin::HookUsage::GenerateBundle
            | rolldown_plugin::HookUsage::RenderChunk
            | rolldown_plugin::HookUsage::WriteBundle
            | rolldown_plugin::HookUsage::RenderStart
            | rolldown_plugin::HookUsage::CloseBundle
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
            Ok(host
                .resolve_id(&spec, &importer)
                .await
                .ok()
                .flatten()
                .map(HookResolveIdOutput::from_id))
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
            Ok(host
                .load(&id)
                .await
                .ok()
                .flatten()
                .map(|code| HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }))
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let host = Arc::clone(&self.host);
        let code = args.code.to_string();
        let id = args.id.to_string();
        async move {
            match host.transform(&code, &id).await {
                Ok((out, _)) if out != code => Ok(Some(HookTransformOutput {
                    code: Some(out),
                    ..Default::default()
                })),
                _ => Ok(None),
            }
        }
    }

    async fn generate_bundle(
        &self,
        _ctx: &PluginContext,
        args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        if !self.host.has_generate_bundle().await {
            return Ok(());
        }
        let bundle_json = serialize_bundle(args.bundle);
        if let Ok(Some(mutated)) = self.host.generate_bundle(&bundle_json, args.is_write).await {
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
        let _ = self.host.write_bundle(&bundle_json, true).await;
        Ok(())
    }

    async fn render_start(
        &self,
        _ctx: &PluginContext,
        _args: &rolldown_plugin::HookRenderStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        let _ = self.host.render_start().await;
        Ok(())
    }

    async fn close_bundle(
        &self,
        _ctx: &PluginContext,
        _args: Option<&rolldown_plugin::HookCloseBundleArgs<'_>>,
    ) -> rolldown_plugin::HookNoopReturn {
        let _ = self.host.close_bundle().await;
        Ok(())
    }
}

fn serialize_rendered_chunk(chunk: &rolldown_common::RollupRenderedChunk) -> String {
    serde_json::json!({
        "type": "chunk",
        "fileName": chunk.filename.to_string(),
        "name": chunk.name.to_string(),
        "isEntry": chunk.is_entry,
        "isDynamicEntry": chunk.is_dynamic_entry,
        "imports": chunk.imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
    })
    .to_string()
}

fn serialize_bundle(bundle: &[rolldown_common::Output]) -> String {
    use rolldown_common::{Output, StrOrBytes};
    let mut map = serde_json::Map::new();
    for out in bundle {
        match out {
            Output::Chunk(c) => {
                map.insert(
                    c.filename.to_string(),
                    serde_json::json!({
                        "type": "chunk",
                        "fileName": c.filename.to_string(),
                        "name": c.name.to_string(),
                        "isEntry": c.is_entry,
                        "code": c.code,
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
                        "source": source,
                    }),
                );
            }
        }
    }
    serde_json::Value::Object(map).to_string()
}

fn apply_bundle_mutations(bundle: &mut [rolldown_common::Output], json: &str) {
    use rolldown_common::{Output, StrOrBytes};
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) else {
        return;
    };
    for out in bundle.iter_mut() {
        match out {
            Output::Chunk(c) => {
                if let Some(code) = map
                    .get(c.filename.as_str())
                    .and_then(|v| v.get("code"))
                    .and_then(|x| x.as_str())
                {
                    if c.code != code {
                        Arc::make_mut(c).code = code.to_string();
                    }
                }
            }
            Output::Asset(a) => {
                if let Some(src) = map
                    .get(a.filename.as_str())
                    .and_then(|v| v.get("source"))
                    .and_then(|x| x.as_str())
                {
                    if a.source.as_bytes() != src.as_bytes() {
                        Arc::make_mut(a).source = StrOrBytes::Str(src.to_string());
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

pub async fn build(
    root: PathBuf,
    out: Option<PathBuf>,
    ssr: Option<String>,
    mode: &str,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;

    let mut config =
        oj_config::load_with(&root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, &root);
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
    let minify = build_cfg.minify.unwrap_or(true);
    let sourcemap = build_cfg.sourcemap.unwrap_or(false);

    if let Some(entry) = ssr.or_else(|| build_cfg.ssr.clone()) {
        return build_ssr_app(
            &root,
            &out_dir,
            &entry,
            minify,
            sourcemap,
            build_cfg.prerender.clone(),
        )
        .await;
    }

    if let Some(lib) = build_cfg.lib.clone() {
        return build_library(&root, &out_dir, lib, minify, sourcemap).await;
    }

    let html_path = root.join("index.html");
    let html = fs::read_to_string(&html_path)
        .with_context(|| format!("no index.html in {}", root.display()))?;

    let entries = module_script_srcs(&html);
    if entries.is_empty() {
        bail!("index.html has no <script type=\"module\" src=...> entry");
    }

    let base = normalize_base(config.base.as_deref().unwrap_or("/"));

    let _ = fs::remove_dir_all(&out_dir);
    fs::create_dir_all(&out_dir)?;

    let started = Instant::now();
    let inputs: Vec<InputItem> = entries
        .iter()
        .map(|entry| InputItem {
            name: Some(
                Path::new(entry)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("entry")
                    .to_string(),
            ),
            import: format!(
                ".{}",
                oj_server::html_entry_src(entry).unwrap_or_else(|| entry.clone())
            ),
            ..Default::default()
        })
        .collect();

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
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_start().await {
            eprintln!("oj build: plugin buildStart failed: {e}");
        }
        oj_plugins.push(Arc::new(OjUserPlugin::new(Arc::clone(host))));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(&root),
        inline_limit: assets_inline_limit_of(&config),
        client: true,
        css_code_split: css_split,
    }));
    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(inputs),
            transform: target_transform(&config),
            code_splitting: manual_chunks(ro_opts),
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
            minify: Some(RawMinifyOptions::Bool(
                oj_config::environment_build_bool(&config, "client", "minify").unwrap_or(minify),
            )),
            sourcemap: oj_config::environment_build_bool(&config, "client", "sourcemap")
                .unwrap_or(sourcemap)
                .then_some(SourceMapType::File),
            define: Some({
                let env = oj_env::load(&root, mode);
                let mut pairs: Vec<(String, String)> =
                    vec![("process.env.NODE_ENV".into(), "'production'".into())];
                pairs.extend(oj_env::import_meta_env_defines(
                    &env, mode, false, &base, "VITE_",
                ));
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "client"));
                pairs.into_iter().collect()
            }),
            ..Default::default()
        })
        .build()
        .map_err(|errs| anyhow::anyhow!("rolldown init failed: {errs:?}"))?;

    let output = bundler
        .write()
        .await
        .map_err(|errs| anyhow::anyhow!("build failed:\n{errs:?}"))?;
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("close failed:\n{errs:?}"))?;

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end().await {
            eprintln!("oj build: plugin buildEnd failed: {e}");
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

    // %VITE_*% / import.meta.env substitution in index.html (Vite's htmlEnvHook),
    // before the plugin transformIndexHtml below.
    let html_env = {
        let env = oj_env::load(&root, mode);
        let mut defines = oj_env::import_meta_env_defines(&env, mode, false, &base, "VITE_");
        defines.extend(oj_config::config_defines(&config));
        oj_env::html_env_map(&defines)
    };
    let mut rewritten_html = oj_env::replace_html_env(&html, &html_env);
    let mut emitted: Vec<(String, usize)> = Vec::new();
    let mut manifest_entries: Vec<ManifestEntry> = Vec::new();
    let mut imports_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut entry_files: Vec<String> = Vec::new();
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
                    for entry in &entries {
                        let resolved =
                            oj_server::html_entry_src(entry).unwrap_or_else(|| entry.clone());
                        let entry_abs = root.join(resolved.trim_start_matches('/'));
                        if Path::new(facade.as_ref()) == entry_abs.as_path() {
                            rewritten_html = rewritten_html
                                .replace(entry.as_str(), &with_base(&filename, &base));
                        }
                    }
                }
            }
        } else if let rolldown_common::Output::Asset(asset) = asset {
            emitted.push((asset.filename.to_string(), asset.source.as_bytes().len()));
        }
    }

    // Inject <link rel="modulepreload"> for each entry's transitively-imported
    // chunks so the browser fetches them in parallel instead of discovering them
    // one waterfall level at a time.
    let mut preloads: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in &entry_files {
        for dep in transitive_imports(entry, &imports_map) {
            preloads.insert(dep);
        }
    }
    if !preloads.is_empty() {
        let links = preloads
            .iter()
            .map(|f| {
                format!(
                    "<link rel=\"modulepreload\" href=\"{}\" />",
                    with_base(f, &base)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        rewritten_html = match rewritten_html.find("</head>") {
            Some(i) => format!(
                "{}{}\n{}",
                &rewritten_html[..i],
                links,
                &rewritten_html[i..]
            ),
            None => format!("{links}\n{rewritten_html}"),
        };
    }

    for href in link_hrefs(&html) {
        let src = root.join(href.trim_start_matches('/'));
        if src.is_file() {
            let dest = out_dir.join(href.trim_start_matches('/'));
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let source = fs::read_to_string(&src)?;
            if oj_server::sidecar::is_tailwind_css(&source) {
                let css = expand_css_via_sidecar(&root, &src)?;
                let minified = oj_css::compile_css(href.as_str(), &css, true)
                    .map_err(|e| anyhow::anyhow!(e))?;
                fs::write(&dest, minified.css)?;
                continue;
            }
            fs::copy(&src, &dest)?;
        }
    }

    if !css_split {
        let mut css_entries = collected_css.lock().unwrap().clone();
        if !css_entries.is_empty() {
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
                    rebase_css_urls(&css, &dir, &out_dir, &base, &mut emitted, &mut seen_assets)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let hash = content_hash(combined.as_bytes());
            let css_name = format!("assets/style-{}.css", &hash[..8]);
            fs::write(out_dir.join(&css_name), &combined)?;
            emitted.push((css_name.clone(), combined.len()));
            let link = format!(
                "<link rel=\"stylesheet\" href=\"{}\" />",
                with_base(&css_name, &base)
            );
            rewritten_html = match rewritten_html.find("</head>") {
                Some(idx) => format!(
                    "{}{}\n{}",
                    &rewritten_html[..idx],
                    link,
                    &rewritten_html[idx..]
                ),
                None => format!("{link}\n{rewritten_html}"),
            };
            for entry in &mut manifest_entries {
                if entry.is_entry {
                    entry.css.push(css_name.clone());
                }
            }
        }
    } else {
        emit_split_css(
            &output,
            &collected_css,
            &entry_files,
            &imports_map,
            &out_dir,
            &root,
            &base,
            &mut emitted,
            &mut manifest_entries,
            &mut rewritten_html,
        )?;
    }

    fs::create_dir_all(out_dir.join(".vite"))?;
    fs::write(
        out_dir.join(".vite").join("manifest.json"),
        serde_json::to_string_pretty(&build_manifest(&manifest_entries))?,
    )?;

    if let Some(host) = &plugin_host {
        if let Ok(out) = host.transform_index_html(&rewritten_html).await {
            rewritten_html = out;
        }
    }
    fs::write(out_dir.join("index.html"), rewritten_html)?;

    let public_dir = config
        .public_dir
        .as_ref()
        .map(|p| root.join(p))
        .unwrap_or_else(|| root.join("public"));
    copy_public_dir(&public_dir, &out_dir)?;

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
    scan_attrs(html, "<script", "src=\"")
        .into_iter()
        .filter(|src| oj_server::html_entry_src(src).is_some())
        .collect()
}

fn link_hrefs(html: &str) -> Vec<String> {
    scan_attrs(html, "<link", "href=\"")
        .into_iter()
        .filter(|href| href.starts_with('/'))
        .collect()
}

fn scan_attrs(html: &str, tag_prefix: &str, attr_prefix: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (start, _) in html.match_indices(tag_prefix) {
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        if tag_prefix == "<script" && !tag.contains("type=\"module\"") {
            continue;
        }
        if let Some(at) = tag.find(attr_prefix) {
            let rest = &tag[at + attr_prefix.len()..];
            if let Some(close) = rest.find('"') {
                values.push(rest[..close].to_string());
            }
        }
    }
    values
}

pub(crate) async fn build_ssr(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    sourcemap: bool,
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
        oj_config::load_with(root, "build", "production").map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root);
    let ssr_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &ssr_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "ssr",
        "production",
    )
    .await;

    let external = IsExternal::Fn(Some(Arc::new(
        |spec: &str, _importer, is_resolved: bool| {
            let ext = is_resolved && spec.contains("node_modules");
            Box::pin(async move { Ok(ext) })
        },
    )));

    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_start().await {
            eprintln!("oj build (ssr): plugin buildStart failed: {e}");
        }
        oj_plugins.push(Arc::new(OjUserPlugin::new(Arc::clone(host))));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(root),
        inline_limit: assets_inline_limit_of(&config),
        client: false,
        css_code_split: false,
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
            transform: target_transform(&config),
            platform: Some(Platform::Node),
            external: Some(external),
            format: Some(OutputFormat::Esm),
            entry_filenames: Some(format!("{stem}.mjs").into()),
            chunk_filenames: Some(format!("{stem}-[hash].mjs").into()),
            minify: Some(RawMinifyOptions::Bool(
                oj_config::environment_build_bool(&config, "ssr", "minify").unwrap_or(false),
            )),
            sourcemap: oj_config::environment_build_bool(&config, "ssr", "sourcemap")
                .unwrap_or(sourcemap)
                .then_some(SourceMapType::File),
            define: Some({
                let mut pairs = vec![
                    (
                        "process.env.NODE_ENV".to_string(),
                        "'production'".to_string(),
                    ),
                    ("import.meta.env.SSR".to_string(), "true".to_string()),
                    ("import.meta.env.PROD".to_string(), "true".to_string()),
                    ("import.meta.env.DEV".to_string(), "false".to_string()),
                    (
                        "import.meta.env.MODE".to_string(),
                        "\"production\"".to_string(),
                    ),
                    ("import.meta.env.BASE_URL".to_string(), "\"/\"".to_string()),
                ];
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "ssr"));
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
            eprintln!("oj build (ssr): plugin buildEnd failed: {e}");
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

async fn build_server_fns(root: &Path, out_dir: &Path) -> anyhow::Result<()> {
    use rolldown::{IsExternal, Platform};
    let entry_path = root.join("_oj_server_fns_entry.tsx");
    fs::write(&entry_path, OJ_SERVER_FNS_JS)?;
    let collected: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let external = IsExternal::Fn(Some(Arc::new(|spec: &str, _i, resolved: bool| {
        let ext = resolved && spec.contains("node_modules");
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
                css_code_split: false,
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
                define: Some(
                    vec![
                        (
                            "process.env.NODE_ENV".to_string(),
                            "'production'".to_string(),
                        ),
                        ("import.meta.env.SSR".to_string(), "true".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                ),
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
    minify: bool,
    sourcemap: bool,
    prerender: Option<Vec<String>>,
) -> anyhow::Result<()> {
    let _ = fs::remove_dir_all(out_dir);
    fs::create_dir_all(out_dir)?;

    build_ssr(root, out_dir, entry, sourcemap).await?;

    let Some(client_entry) = derive_client_entry(root, entry) else {
        println!("oj build (ssr): server bundle only (no *-client sibling to hydrate)");
        return Ok(());
    };
    let (js, css) = build_client_entry(root, out_dir, &client_entry, minify, sourcemap).await?;

    build_server_fns(root, out_dir).await?;
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
    minify: bool,
    sourcemap: bool,
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
        oj_config::load_with(root, "build", "production").map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root);
    let client_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &client_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
        "production",
    )
    .await;
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_start().await {
            eprintln!("oj build (client): plugin buildStart failed: {e}");
        }
        oj_plugins.push(Arc::new(OjUserPlugin::new(Arc::clone(host))));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(root),
        inline_limit: assets_inline_limit_of(&config),
        client: true,
        css_code_split: false,
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
            transform: target_transform(&config),
            entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
            chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
            minify: Some(RawMinifyOptions::Bool(
                oj_config::environment_build_bool(&config, "client", "minify").unwrap_or(minify),
            )),
            sourcemap: oj_config::environment_build_bool(&config, "client", "sourcemap")
                .unwrap_or(sourcemap)
                .then_some(SourceMapType::File),
            define: Some({
                let env = oj_env::load(root, "production");
                let mut pairs = vec![(
                    "process.env.NODE_ENV".to_string(),
                    "'production'".to_string(),
                )];
                pairs.extend(oj_env::import_meta_env_defines(
                    &env,
                    "production",
                    false,
                    "/",
                    "VITE_",
                ));
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "client"));
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
            eprintln!("oj build (client): plugin buildEnd failed: {e}");
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
    lib: oj_config::LibConfig,
    minify: bool,
    sourcemap: bool,
) -> anyhow::Result<()> {
    let entry_import = if lib.entry.starts_with('.') {
        lib.entry.clone()
    } else {
        format!("./{}", lib.entry)
    };
    let file_name = lib.file_name.clone().unwrap_or_else(|| {
        Path::new(&lib.entry)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("index")
            .to_string()
    });
    let formats = lib.formats.clone().unwrap_or_else(|| vec!["es".into()]);

    let _ = fs::remove_dir_all(out_dir);
    fs::create_dir_all(out_dir)?;
    let started = Instant::now();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut emitted: Vec<(String, usize)> = Vec::new();

    for fmt in &formats {
        let (ext, needs_name) = lib_format(fmt)
            .ok_or_else(|| anyhow::anyhow!("unknown lib format: {fmt} (es, cjs, umd, iife)"))?;
        let format = match fmt.as_str() {
            "es" | "esm" => OutputFormat::Esm,
            "cjs" => OutputFormat::Cjs,
            "umd" => OutputFormat::Umd,
            _ => OutputFormat::Iife,
        };
        if needs_name && lib.name.is_none() {
            bail!("build.lib.name is required for the '{fmt}' format");
        }

        let mut bundler = BundlerBuilder::default()
            .with_plugins(vec![Arc::new(OjCssPlugin {
                collected: Arc::clone(&collected_css),
                root: root.to_path_buf(),
                has_postcss: oj_server::has_postcss_config(root),
                inline_limit: 4096,
                client: true,
                css_code_split: false,
            })])
            .with_options(BundlerOptions {
                input: Some(vec![InputItem {
                    name: Some(file_name.clone()),
                    import: entry_import.clone(),
                    ..Default::default()
                }]),
                cwd: Some(root.to_path_buf()),
                dir: Some(out_dir.display().to_string()),
                format: Some(format),
                name: lib.name.clone(),
                entry_filenames: Some(format!("{file_name}.{ext}").into()),
                chunk_filenames: Some(format!("{file_name}-[hash].{ext}").into()),
                minify: Some(RawMinifyOptions::Bool(minify)),
                sourcemap: sourcemap.then_some(SourceMapType::File),
                define: Some(
                    std::iter::once((
                        "process.env.NODE_ENV".to_string(),
                        "'production'".to_string(),
                    ))
                    .collect(),
                ),
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
        let css_name = format!("{file_name}.css");
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

fn lib_format(fmt: &str) -> Option<(&'static str, bool)> {
    match fmt {
        "es" | "esm" => Some(("js", false)),
        "cjs" => Some(("cjs", false)),
        "umd" => Some(("umd.js", true)),
        "iife" => Some(("iife.js", true)),
        _ => None,
    }
}

fn normalize_base(base: &str) -> String {
    if base.is_empty() || base == "./" {
        return "/".to_string();
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

fn with_base(filename: &str, base: &str) -> String {
    format!("{base}{}", filename.trim_start_matches('/'))
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

/// The hash in a content-addressed filename.
///
/// blake3 rather than `DefaultHasher`: the standard library makes no promise
/// about that hasher's output across releases, so asset filenames could change
/// -- invalidating every caching header a deploy relies on -- purely because oj
/// was rebuilt with a newer Rust.
fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

/// Emit a CSS-referenced asset (font/image) under a content hash and return its
/// base-prefixed URL, or None to leave the `url()` untouched (data:/absolute/external).
fn emit_css_url(
    inner: &str,
    css_dir: &Path,
    out_dir: &Path,
    base: &str,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    if inner.is_empty()
        || inner.starts_with("data:")
        || inner.starts_with("http://")
        || inner.starts_with("https://")
        || inner.starts_with("//")
        || inner.starts_with('#')
        || inner.starts_with('/')
    {
        return None;
    }
    let cut = inner.find(['?', '#']).unwrap_or(inner.len());
    let (clean, suffix) = inner.split_at(cut);
    let abs = css_dir.join(clean).canonicalize().ok()?;
    if let Some(url) = seen.get(&abs) {
        return Some(format!("{url}{suffix}"));
    }
    let data = std::fs::read(&abs).ok()?;
    let hash = content_hash(&data);
    let stem = abs.file_stem().and_then(|s| s.to_str()).unwrap_or("asset");
    let ext = abs
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let name = format!("assets/{}-{}{}", sanitize_asset_name(stem), &hash[..8], ext);
    let dest = out_dir.join(&name);
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(&dest, &data).ok()?;
    emitted.push((name.clone(), data.len()));
    let url = with_base(&name, base);
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
        match emit_css_url(inner, css_dir, out_dir, base, emitted, seen) {
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

/// `build.cssCodeSplit`: emit one stylesheet per chunk that imports CSS. The
/// entry chunk and every chunk reachable from it via static imports get their
/// CSS linked render-blocking in the HTML (no FOUC); async-only chunks carry a
/// tiny injector that appends their stylesheet `<link>` when the chunk runs.
#[allow(clippy::too_many_arguments)]
fn emit_split_css(
    output: &rolldown::BundleOutput,
    collected_css: &Arc<Mutex<Vec<(String, String)>>>,
    entry_files: &[String],
    imports_map: &std::collections::HashMap<String, Vec<String>>,
    out_dir: &Path,
    root: &Path,
    base: &str,
    emitted: &mut Vec<(String, usize)>,
    manifest_entries: &mut [ManifestEntry],
    rewritten_html: &mut String,
) -> anyhow::Result<()> {
    let css_map: std::collections::HashMap<String, String> =
        collected_css.lock().unwrap().iter().cloned().collect();
    if css_map.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(out_dir.join("assets"))?;
    let mut seen_assets: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();

    // Chunks reachable from an HTML entry through static imports only: their CSS
    // is safe to link render-blocking. Anything else loads asynchronously.
    let mut sync_chunks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in entry_files {
        sync_chunks.insert(entry.clone());
        sync_chunks.extend(transitive_imports(entry, imports_map));
    }

    // chunk filename -> emitted css filename
    let mut chunk_css: Vec<(String, String)> = Vec::new();
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
                    emitted,
                    &mut seen_assets,
                ));
            }
        }
        if css.is_empty() {
            continue;
        }
        let hash = content_hash(css.as_bytes());
        let css_name = format!(
            "assets/{}-{}.css",
            sanitize_asset_name(&chunk.name),
            &hash[..8]
        );
        fs::write(out_dir.join(&css_name), &css)?;
        emitted.push((css_name.clone(), css.len()));
        for entry in manifest_entries.iter_mut() {
            if entry.file == chunk.filename.as_str() {
                entry.css.push(css_name.clone());
            }
        }
        chunk_css.push((chunk.filename.to_string(), css_name));
    }

    // Render-blocking links for entry + statically-imported chunk CSS.
    let mut linked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut links = String::new();
    for (chunk_file, css_name) in &chunk_css {
        if sync_chunks.contains(chunk_file) && linked.insert(css_name.clone()) {
            links.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\" />\n",
                with_base(css_name, base)
            ));
        }
    }
    if !links.is_empty() {
        let links = links.trim_end();
        *rewritten_html = match rewritten_html.find("</head>") {
            Some(idx) => format!(
                "{}{}\n{}",
                &rewritten_html[..idx],
                links,
                &rewritten_html[idx..]
            ),
            None => format!("{links}\n{rewritten_html}"),
        };
    }

    // Async-only chunks self-inject their stylesheet when evaluated.
    for (chunk_file, css_name) in &chunk_css {
        if sync_chunks.contains(chunk_file) {
            continue;
        }
        let path = out_dir.join(chunk_file);
        let Ok(code) = fs::read_to_string(&path) else {
            continue;
        };
        let href = serde_json::Value::String(with_base(css_name, base));
        let inject = format!(
            "(function(){{var u={href};if(!document.querySelector('link[rel=\"stylesheet\"][href=\"'+u+'\"]')){{var l=document.createElement('link');l.rel='stylesheet';l.href=u;document.head.appendChild(l);}}}})();\n"
        );
        fs::write(&path, format!("{inject}{code}"))?;
    }
    Ok(())
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
        assert_eq!(lib_format("es"), Some(("js", false)));
        assert_eq!(lib_format("esm"), Some(("js", false)));
        assert_eq!(lib_format("cjs"), Some(("cjs", false)));
        assert_eq!(lib_format("umd"), Some(("umd.js", true)));
        assert_eq!(lib_format("iife"), Some(("iife.js", true)));
        assert_eq!(lib_format("amd"), None);
    }

    #[test]
    fn base_normalization_and_application() {
        assert_eq!(normalize_base("/"), "/");
        assert_eq!(normalize_base(""), "/");
        assert_eq!(normalize_base("./"), "/");
        assert_eq!(normalize_base("app"), "/app/");
        assert_eq!(normalize_base("/app"), "/app/");
        assert_eq!(normalize_base("/app/"), "/app/");
        assert_eq!(with_base("assets/x-h.js", "/"), "/assets/x-h.js");
        assert_eq!(with_base("assets/x-h.js", "/app/"), "/app/assets/x-h.js");
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
    fn link_hrefs_collects_only_absolute_hrefs() {
        let html = r#"<html><head>
          <link rel="stylesheet" href="/assets/app.css">
          <link rel="icon" href="favicon.ico">
          <link rel="modulepreload" href="/assets/chunk.js">
        </head></html>"#;
        let hrefs = link_hrefs(html);
        assert!(hrefs.contains(&"/assets/app.css".to_string()), "{hrefs:?}");
        assert!(hrefs.contains(&"/assets/chunk.js".to_string()), "{hrefs:?}");
        assert!(
            !hrefs.iter().any(|h| h.contains("favicon")),
            "relative href filtered: {hrefs:?}"
        );
    }
}
