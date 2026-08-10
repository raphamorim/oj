// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build`: production build via embedded Rolldown.
//!
//! Per the research decision, the prod linker (tree shaking, chunking,
//! minification) is the least differentiated multi-month component — we
//! embed Rolldown 1.x (MIT) instead of rebuilding it. oj owns the app-shaped
//! parts: HTML entry discovery, NODE_ENV, hashed-asset HTML rewriting, and
//! the summary UX.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rolldown::{
    BundlerBuilder, BundlerOptions, InputItem, OutputFormat, RawMinifyOptions, SourceMapType,
};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookRenderChunkArgs, HookRenderChunkOutput,
    HookRenderChunkReturn, HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn,
    HookTransformArgs, HookTransformOutput, HookTransformOutputMap, HookTransformReturn, Plugin,
    PluginContext, SharedLoadPluginContext, SharedTransformPluginContext,
};
use rolldown_plugin::__inner::SharedPluginable;
use oj_server::plugins::PluginHost;

/// The oj build plugin: loads `.css`/`.scss` imports as JS stubs (CSS Modules
/// export their scoped class map) collecting compiled CSS for one emitted
/// stylesheet, and — via `transform` — expands `import.meta.glob` (Rolldown
/// has no native glob), keeping prod builds in sync with dev.
#[derive(Debug)]
struct OjCssPlugin {
    collected: Arc<Mutex<Vec<(String, String)>>>,
    /// App root, so CSS-module class names hash from the same root-relative id
    /// (`/src/x.module.css`) the dev server uses — keeping `oj dev`, `oj build`,
    /// and the SSR/client bundles all in agreement (matters for hydration).
    root: PathBuf,
    /// App has a postcss.config — run all .css through the sidecar (PostCSS),
    /// not just Tailwind-flagged css (parity with the dev server).
    has_postcss: bool,
}

impl Plugin for OjCssPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:build")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Load | rolldown_plugin::HookUsage::Transform
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let id = args.id.to_string();
        let code = args.code.to_string();
        async move {
            if !code.contains("import.meta.glob") {
                return Ok(None);
            }
            let expanded = oj_compiler::glob::expand_source(&code, std::path::Path::new(&id));
            Ok(Some(rolldown_plugin::HookTransformOutput {
                code: Some(expanded),
                ..Default::default()
            }))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let id = args.id.to_string();
        let collected = Arc::clone(&self.collected);
        let root = self.root.clone();
        async move {
            let path = id.split('?').next().unwrap_or(&id);
            if !(path.ends_with(".css") || oj_css::is_sass(path)) {
                return Ok(None);
            }
            let mut source = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
            // Sass/SCSS -> CSS first (sibling @use/@import resolve from dir).
            if oj_css::is_sass(path) {
                let dir = std::path::Path::new(path).parent();
                source = oj_css::compile_sass(&source, dir).map_err(|e| anyhow::anyhow!(e))?;
            }
            // Run css through the CSS sidecar (the app's postcss.config, or the
            // Tailwind v4 API) exactly like the dev server: for Tailwind-flagged
            // css always, and for any .css when the app has a postcss.config
            // (arbitrary PostCSS). Plain css with no postcss config goes straight
            // to Lightning below.
            if oj_server::sidecar::is_tailwind_css(&source)
                || (self.has_postcss && path.ends_with(".css"))
            {
                source = expand_css_via_sidecar(&root, std::path::Path::new(path))?;
            }
            // Hash class names from the dev server's root-relative id form.
            let css_id = match std::path::Path::new(path).strip_prefix(&root) {
                Ok(rel) => format!("/{}", rel.display()),
                Err(_) => path.to_string(),
            };
            let output = oj_css::compile_css(&css_id, &source, true)
                .map_err(|e| anyhow::anyhow!(e))?;
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
            collected.lock().unwrap().push((path.to_string(), output.css));
            Ok(Some(rolldown_plugin::HookLoadOutput {
                code: arcstr::ArcStr::from(js),
                module_type: Some(rolldown_common::ModuleType::Js),
                ..Default::default()
            }))
        }
    }
}

/// Expand `@tailwind`/`@apply` (and any `postcss.config.*` plugins) in a CSS
/// file via oj's one-shot CSS sidecar — the same sidecar the dev server runs,
/// so `oj dev` and `oj build` produce identical Tailwind output. Returns the
/// fully-expanded CSS.
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
        // cwd = app root so Tailwind v3 finds tailwind.config.* + content globs
        // (matches the persistent dev sidecar's current_dir(root)).
        .current_dir(root)
        .output()
        .context("node not found for tailwind/postcss build")?;
    if !out.status.success() {
        bail!("css build failed for {}: {}", css_file.display(), String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Recursively copy the contents of `src` into `dest` (Vite's publicDir copy).
/// No-op if `src` doesn't exist. Existing files (e.g. a generated index.html)
/// are not clobbered by same-named public files.
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

/// Bridges the Node plugin host into the Rolldown build, so the same
/// Vite/Rollup-style `resolveId`/`load`/`transform` hooks that run in the dev
/// server also run in `oj build`. Runs before `OjCssPlugin` so user transforms
/// see raw source and user resolveId/load win for virtual ids.
#[derive(Debug)]
struct OjUserPlugin {
    host: Arc<PluginHost>,
    /// Cached `has_render_chunk()` — renderChunk fires per chunk, so we resolve
    /// it once instead of round-tripping to Node for every chunk.
    render_chunk_enabled: Arc<tokio::sync::OnceCell<bool>>,
}

impl OjUserPlugin {
    fn new(host: Arc<PluginHost>) -> Self {
        Self { host, render_chunk_enabled: Arc::new(tokio::sync::OnceCell::new()) }
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
            Ok(host.load(&id).await.ok().flatten().map(|code| HookLoadOutput {
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
                Ok(out) if out != code => Ok(Some(HookTransformOutput {
                    code: Some(out),
                    ..Default::default()
                })),
                _ => Ok(None),
            }
        }
    }

    // Output-phase hook: the plugins see the finished bundle (keyed by
    // fileName), may read it, mutate chunk `code` / asset `source`, or emitFile
    // new assets (collected separately). Skipped entirely when no plugin
    // defines generateBundle, so builds that don't use it pay nothing.
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

    // Per-chunk output hook: chain the plugins' renderChunk over each chunk's
    // rendered code. Cached `has_render_chunk` avoids a Node round-trip per
    // chunk when nothing uses it. Sourcemap is dropped (Null) on a change.
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
            let on = *enabled.get_or_init(|| async { host.has_render_chunk().await }).await;
            if !on {
                return Ok(None);
            }
            match host.render_chunk(&code, &chunk_json).await {
                Ok(Some(out)) if out != *code => {
                    Ok(Some(HookRenderChunkOutput { code: out, map: HookTransformOutputMap::Null }))
                }
                _ => Ok(None),
            }
        }
    }

    // Post-write hook: files are already on disk, so this is a read-only
    // notification (plugins do side effects like extra fs writes / logging).
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

    // Output phase begins (after buildEnd, before renderChunk). Side effect.
    async fn render_start(
        &self,
        _ctx: &PluginContext,
        _args: &rolldown_plugin::HookRenderStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        let _ = self.host.render_start().await;
        Ok(())
    }

    // The very last hook, after everything is written. Side effect.
    async fn close_bundle(
        &self,
        _ctx: &PluginContext,
        _args: Option<&rolldown_plugin::HookCloseBundleArgs<'_>>,
    ) -> rolldown_plugin::HookNoopReturn {
        let _ = self.host.close_bundle().await;
        Ok(())
    }
}

/// A Rollup RenderedChunk as the plugin host expects it (metadata only; the
/// code is passed separately as the hook's first argument).
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

/// Serialize the Rolldown output bundle to a Rollup-shaped `bundle` object
/// (JSON keyed by fileName) for the plugin host. Binary asset sources are sent
/// as null (readable metadata only; not mutable through this bridge).
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

/// Apply a plugin's `generateBundle` edits back onto the Rolldown output: only
/// `code`/`source` changes to existing entries (COW via `Arc::make_mut`). New
/// entries (use `this.emitFile`) and deletions are not applied.
fn apply_bundle_mutations(bundle: &mut [rolldown_common::Output], json: &str) {
    use rolldown_common::{Output, StrOrBytes};
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) else {
        return;
    };
    for out in bundle.iter_mut() {
        match out {
            Output::Chunk(c) => {
                if let Some(code) =
                    map.get(c.filename.as_str()).and_then(|v| v.get("code")).and_then(|x| x.as_str())
                {
                    if c.code != code {
                        Arc::make_mut(c).code = code.to_string();
                    }
                }
            }
            Output::Asset(a) => {
                if let Some(src) =
                    map.get(a.filename.as_str()).and_then(|v| v.get("source")).and_then(|x| x.as_str())
                {
                    if a.source.as_bytes() != src.as_bytes() {
                        Arc::make_mut(a).source = StrOrBytes::Str(src.to_string());
                    }
                }
            }
        }
    }
}

/// Spawn the plugin host for a build (`command: "build"`) if the app declares
/// plugins. Returned so the build can both add it as a Rolldown plugin and run
/// transformIndexHtml on the emitted HTML.
async fn user_plugin_host(
    root: &Path,
    base: &str,
    define: &serde_json::Value,
    environments: &serde_json::Value,
    env_name: &str,
) -> Option<Arc<PluginHost>> {
    let file = oj_server::plugins::plugins_file(root)?;
    let config = serde_json::json!({
        "config": { "root": root.display().to_string(), "base": base, "mode": "production", "command": "build", "define": define, "environments": environments },
        "env": { "command": "build", "mode": "production" },
        // Which Vite environment this build represents ("client" or "ssr").
        "environment": { "name": env_name, "mode": "production" },
    })
    .to_string();
    match PluginHost::spawn(root, &file, &config).await {
        Ok(host) => {
            println!("oj build ({env_name}): plugins from {}", file.file_name().unwrap().to_string_lossy());
            Some(host)
        }
        Err(e) => {
            eprintln!("oj build ({env_name}): plugin host failed to start: {e}");
            None
        }
    }
}

pub async fn build(root: PathBuf, out: Option<PathBuf>, ssr: Option<String>) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;

    let config = oj_config::load(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
    let build_cfg = config.build.clone().unwrap_or_default();
    // Precedence: CLI --out > config build.outDir > "dist".
    let out = out
        .or_else(|| build_cfg.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dist"));
    let out_dir = if out.is_absolute() { out } else { root.join(&out) };
    let minify = build_cfg.minify.unwrap_or(true);
    let sourcemap = build_cfg.sourcemap.unwrap_or(true);
    if build_cfg.target.is_some() {
        eprintln!("oj build: note: build.target is accepted but not yet applied");
    }

    // SSR mode: build the server bundle, a client hydration bundle, and a
    // streaming production server that ties them together.
    if let Some(entry) = ssr.or_else(|| build_cfg.ssr.clone()) {
        return build_ssr_app(&root, &out_dir, &entry, minify, sourcemap).await;
    }

    // Library mode: build a distributable, not an app — no index.html.
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
            import: format!(".{entry}"),
            ..Default::default()
        })
        .collect();

    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let plugin_host = user_plugin_host(
        &root,
        &base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
    )
    .await;
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_start().await {
            eprintln!("oj build: plugin buildStart failed: {e}");
        }
        oj_plugins.push(Arc::new(OjUserPlugin::new(Arc::clone(host)))); // before oj:build
    }
    oj_plugins.push(Arc::new(OjCssPlugin { collected: Arc::clone(&collected_css), root: root.to_path_buf(), has_postcss: oj_server::has_postcss_config(&root) }));
    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
        input: Some(inputs),
        cwd: Some(root.clone()),
        dir: Some(out_dir.display().to_string()),
        entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
        chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
        minify: Some(RawMinifyOptions::Bool(minify)),
        sourcemap: sourcemap.then_some(SourceMapType::File),
        define: Some({
            // NODE_ENV plus the app's .env-derived import.meta.env.* values,
            // loaded in production mode. BASE_URL reflects the configured base.
            // Then config `define` + the "client" environment's define overrides.
            let env = oj_env::load(&root, "production");
            let mut pairs: Vec<(String, String)> =
                vec![("process.env.NODE_ENV".into(), "'production'".into())];
            pairs.extend(oj_env::import_meta_env_defines(&env, "production", false, &base, "VITE_"));
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
    // Fire the closeBundle hook (the last output hook; write() alone doesn't).
    bundler.close().await.map_err(|errs| anyhow::anyhow!("close failed:\n{errs:?}"))?;

    // The module graph is complete: buildEnd fires before we emit HTML/manifest.
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end().await {
            eprintln!("oj build: plugin buildEnd failed: {e}");
        }
        // Write any assets plugins emitted via this.emitFile into the output.
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
        // oj's transform plugins intentionally don't emit sourcemaps yet.
        if format!("{warning:?}").contains("SOURCEMAP_BROKEN") {
            continue;
        }
        eprintln!("oj build warning: {warning:?}");
    }

    // Map each entry url to its hashed chunk filename for HTML rewriting.
    let mut rewritten_html = html.clone();
    let mut emitted: Vec<(String, usize)> = Vec::new();
    let mut manifest_entries: Vec<ManifestEntry> = Vec::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            emitted.push((chunk.filename.to_string(), chunk.code.len()));
            if !chunk.is_entry {
                continue;
            }
            let Some(facade) = &chunk.facade_module_id else { continue };
            // Root-relative source path is the Vite manifest key.
            let src = Path::new(facade.as_ref())
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| chunk.name.to_string());
            manifest_entries.push(ManifestEntry {
                name: chunk.name.to_string(),
                file: chunk.filename.to_string(),
                src: src.clone(),
                is_entry: true,
                imports: chunk.imports.iter().map(|i| i.to_string()).collect(),
                css: Vec::new(),
            });
            for entry in &entries {
                let entry_abs = root.join(entry.trim_start_matches('/'));
                if Path::new(facade.as_ref()) == entry_abs.as_path() {
                    rewritten_html =
                        rewritten_html.replace(entry.as_str(), &with_base(&chunk.filename, &base));
                }
            }
        } else if let rolldown_common::Output::Asset(asset) = asset {
            emitted.push((asset.filename.to_string(), asset.source.as_bytes().len()));
        }
    }

    // Stylesheets and other <link href> statics are copied through as-is
    // (hashed CSS pipeline is future work).
    for href in link_hrefs(&html) {
        let src = root.join(href.trim_start_matches('/'));
        if src.is_file() {
            let dest = out_dir.join(href.trim_start_matches('/'));
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let source = fs::read_to_string(&src)?;
            if oj_server::sidecar::is_tailwind_css(&source) {
                // Expand via the CSS sidecar (postcss config / Tailwind v4).
                let css = expand_css_via_sidecar(&root, &src)?;
                let minified = oj_css::compile_css(href.as_str(), &css, true).map_err(|e| anyhow::anyhow!(e))?;
                fs::write(&dest, minified.css)?;
                continue;
            }
            fs::copy(&src, &dest)?;
        }
    }

    // Emit collected css (imports, incl. CSS Modules) as one stylesheet.
    let mut css_entries = collected_css.lock().unwrap().clone();
    if !css_entries.is_empty() {
        css_entries.sort();
        let combined: String =
            css_entries.into_iter().map(|(_, css)| css).collect::<Vec<_>>().join("\n");
        let hash = format!("{:016x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            combined.hash(&mut h);
            h.finish()
        });
        let css_name = format!("assets/style-{}.css", &hash[..8]);
        fs::create_dir_all(out_dir.join("assets"))?;
        fs::write(out_dir.join(&css_name), &combined)?;
        emitted.push((css_name.clone(), combined.len()));
        let link = format!("<link rel=\"stylesheet\" href=\"{}\" />", with_base(&css_name, &base));
        rewritten_html = match rewritten_html.find("</head>") {
            Some(idx) => format!("{}{}\n{}", &rewritten_html[..idx], link, &rewritten_html[idx..]),
            None => format!("{link}\n{rewritten_html}"),
        };
        // The app's css belongs to every entry (no per-entry css splitting yet).
        for entry in &mut manifest_entries {
            entry.css.push(css_name.clone());
        }
    }

    // Vite-compatible manifest for backend integrations (Laravel/Rails/etc.),
    // at the location their plugins expect: dist/.vite/manifest.json.
    fs::create_dir_all(out_dir.join(".vite"))?;
    fs::write(
        out_dir.join(".vite").join("manifest.json"),
        serde_json::to_string_pretty(&build_manifest(&manifest_entries))?,
    )?;

    // Plugin transformIndexHtml runs on the finished document.
    if let Some(host) = &plugin_host {
        if let Ok(out) = host.transform_index_html(&rewritten_html).await {
            rewritten_html = out;
        }
    }
    fs::write(out_dir.join("index.html"), rewritten_html)?;

    // Vite-style publicDir: copy <root>/public/* verbatim to the output root
    // (favicon.ico, robots.txt, static assets). The dev server serves these
    // live; the build must ship them.
    copy_public_dir(&root.join("public"), &out_dir)?;

    println!("oj build: {} in {:?}", out_dir.display(), started.elapsed());
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
        .filter(|src| src.starts_with('/'))
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
        let Some(end) = html[start..].find('>') else { continue };
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

/// Build an SSR server bundle (`build.ssr` / `--ssr`): target Node, keep bare
/// dependencies external (Node resolves them at runtime), emit one ESM
/// `<stem>.mjs`. This is the server-build half of SSR; a dev-server SSR module
/// runner (Environment API) is separate, larger work.
pub(crate) async fn build_ssr(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    sourcemap: bool,
) -> anyhow::Result<()> {
    use rolldown::{IsExternal, Platform};

    let entry_import = if entry.starts_with('.') { entry.to_string() } else { format!("./{entry}") };
    let stem =
        Path::new(entry).file_stem().and_then(|s| s.to_str()).unwrap_or("server").to_string();

    // The caller (`build_ssr_app`) owns wiping the shared out dir; just ensure
    // it exists so the server bundle can sit next to the client assets.
    fs::create_dir_all(out_dir)?;
    let started = Instant::now();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    // User plugins run in the SSR build as the "ssr" environment, so
    // resolveId/load/transform (and applyToEnvironment("ssr")) apply to server
    // modules — parity with the dev SSR runner.
    let config = oj_config::load(root).map_err(|e| anyhow::anyhow!("{e}"))?;
    let ssr_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &ssr_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "ssr",
    )
    .await;

    // Externalize a module once it resolves into node_modules (Node requires
    // those at runtime); aliases (`@/…`) and relative imports resolve to
    // source and stay bundled.
    let external = IsExternal::Fn(Some(Arc::new(|spec: &str, _importer, is_resolved: bool| {
        let ext = is_resolved && spec.contains("node_modules");
        Box::pin(async move { Ok(ext) })
    })));

    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_start().await {
            eprintln!("oj build (ssr): plugin buildStart failed: {e}");
        }
        oj_plugins.push(Arc::new(OjUserPlugin::new(Arc::clone(host))));
    }
    oj_plugins.push(Arc::new(OjCssPlugin { collected: Arc::clone(&collected_css), root: root.to_path_buf(), has_postcss: oj_server::has_postcss_config(root) }));
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
            platform: Some(Platform::Node),
            external: Some(external),
            format: Some(OutputFormat::Esm),
            entry_filenames: Some(format!("{stem}.mjs").into()),
            chunk_filenames: Some(format!("{stem}-[hash].mjs").into()),
            minify: Some(RawMinifyOptions::Bool(false)),
            sourcemap: sourcemap.then_some(SourceMapType::File),
            define: Some({
                let mut pairs = vec![
                    ("process.env.NODE_ENV".to_string(), "'production'".to_string()),
                    ("import.meta.env.SSR".to_string(), "true".to_string()),
                    ("import.meta.env.PROD".to_string(), "true".to_string()),
                    ("import.meta.env.DEV".to_string(), "false".to_string()),
                    ("import.meta.env.MODE".to_string(), "\"production\"".to_string()),
                    ("import.meta.env.BASE_URL".to_string(), "\"/\"".to_string()),
                ];
                // config define + the "ssr" environment's define overrides.
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
    bundler.close().await.map_err(|errs| anyhow::anyhow!("ssr close failed:\n{errs:?}"))?;

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
    println!("oj build (ssr): {} in {:?}", out_dir.display(), started.elapsed());
    emitted.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, bytes) in &emitted {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    Ok(())
}

/// The emitted streaming production SSR server. Imports the server bundle,
/// streams `renderToReadableStream` over a chunked HTTP response with the
/// hashed client script/stylesheet injected, and serves the client assets.
/// `__CLIENT_JS__` / `__CLIENT_CSS__` are filled in at build time.
const SSR_PROD_SERVER: &str = r#"// Generated by `oj build --ssr` — streaming production SSR server.
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, normalize } from "node:path";
import * as entry from "./entry-server.mjs";

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
    // Action (mutation): run it server-side, then revalidate. Compute before
    // writing headers so a throwing loader/action falls to the catch cleanly.
    if (req.method === "POST") {
      if (typeof entry.action === "function") await entry.action(url, await readBody(req));
      if (wantsData) {
        const body = serialize(await load());
        res.writeHead(200, { "content-type": "application/json" });
        return void res.end(body);
      }
      // No-JS form: redirect so the browser re-GETs the updated document.
      return void res.writeHead(303, { location: url }).end();
    }
    // Client data fetch for a navigation.
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

/// Derive the client hydration entry from the server entry by convention:
/// swap "server" -> "client" in the filename, if that sibling exists.
fn derive_client_entry(root: &Path, server_entry: &str) -> Option<String> {
    let file = Path::new(server_entry).file_name()?.to_str()?;
    if !file.contains("server") {
        return None;
    }
    let client_file = file.replace("server", "client");
    let client_rel = match Path::new(server_entry).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => format!("{}/{}", dir.to_string_lossy(), client_file),
        _ => client_file,
    };
    root.join(&client_rel).is_file().then_some(client_rel)
}

/// Full production SSR build: the Node server bundle, a browser client bundle
/// for hydration (from the sibling `*-client.*` entry), and a streaming
/// `server.mjs` that ties them together. Without a client entry, only the
/// server bundle is emitted (no runnable server — nothing to hydrate).
pub(crate) async fn build_ssr_app(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    minify: bool,
    sourcemap: bool,
) -> anyhow::Result<()> {
    let _ = fs::remove_dir_all(out_dir);
    fs::create_dir_all(out_dir)?;

    build_ssr(root, out_dir, entry, sourcemap).await?;

    let Some(client_entry) = derive_client_entry(root, entry) else {
        println!("oj build (ssr): server bundle only (no *-client sibling to hydrate)");
        return Ok(());
    };
    let (js, css) = build_client_entry(root, out_dir, &client_entry, minify, sourcemap).await?;

    let server = SSR_PROD_SERVER
        .replace("__CLIENT_JS__", &js)
        .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""));
    fs::write(out_dir.join("server.mjs"), server)?;
    println!("  {:>9}  server.mjs", human_bytes(SSR_PROD_SERVER.len()));
    println!("  run: node {}", out_dir.join("server.mjs").display());
    Ok(())
}

/// Bundle one browser entry (prod, hashed, minified) into `<out>/assets`,
/// returning the entry's `/assets/<name>-<hash>.js` url and an optional
/// `/assets/style-<hash>.css` url for the collected CSS. Used to build the
/// client hydration bundle for a production SSR app.
async fn build_client_entry(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    minify: bool,
    sourcemap: bool,
) -> anyhow::Result<(String, Option<String>)> {
    let entry_import = if entry.starts_with('.') { entry.to_string() } else { format!("./{entry}") };
    let stem = Path::new(entry).file_stem().and_then(|s| s.to_str()).unwrap_or("client").to_string();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    // User plugins run in the client hydration bundle as the "client"
    // environment (parity with the dev client pipeline), so a shared module can
    // use plugin resolveId/load/transform on both the server and client sides.
    let config = oj_config::load(root).map_err(|e| anyhow::anyhow!("{e}"))?;
    let client_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &client_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
    )
    .await;
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_start().await {
            eprintln!("oj build (client): plugin buildStart failed: {e}");
        }
        oj_plugins.push(Arc::new(OjUserPlugin::new(Arc::clone(host))));
    }
    oj_plugins.push(Arc::new(OjCssPlugin { collected: Arc::clone(&collected_css), root: root.to_path_buf(), has_postcss: oj_server::has_postcss_config(root) }));

    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem { name: Some(stem), import: entry_import, ..Default::default() }]),
            cwd: Some(root.to_path_buf()),
            dir: Some(out_dir.display().to_string()),
            entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
            chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
            minify: Some(RawMinifyOptions::Bool(minify)),
            sourcemap: sourcemap.then_some(SourceMapType::File),
            define: Some({
                let env = oj_env::load(root, "production");
                let mut pairs =
                    vec![("process.env.NODE_ENV".to_string(), "'production'".to_string())];
                pairs.extend(oj_env::import_meta_env_defines(&env, "production", false, "/", "VITE_"));
                // client hydration bundle: config + "client" environment define.
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
    bundler.close().await.map_err(|errs| anyhow::anyhow!("client close failed:\n{errs:?}"))?;

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

    // Emit collected CSS (incl. CSS Modules) as one hashed stylesheet.
    let mut css_entries = collected_css.lock().unwrap().clone();
    let css = if css_entries.is_empty() {
        None
    } else {
        css_entries.sort();
        let combined: String =
            css_entries.into_iter().map(|(_, css)| css).collect::<Vec<_>>().join("\n");
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

/// Build a library (`build.lib`): one Rolldown pass per output format,
/// emitting `<fileName>.<ext>` files plus a single stylesheet for any
/// imported CSS. No HTML, no manifest.
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
            .with_plugins(vec![Arc::new(OjCssPlugin { collected: Arc::clone(&collected_css), root: root.to_path_buf(), has_postcss: oj_server::has_postcss_config(root) })])
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
                    std::iter::once(("process.env.NODE_ENV".to_string(), "'production'".to_string()))
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

    // One stylesheet for any CSS the library imported.
    let css_entries = collected_css.lock().unwrap().clone();
    if !css_entries.is_empty() {
        let combined: String =
            css_entries.into_iter().map(|(_, css)| css).collect::<Vec<_>>().join("\n");
        let css_name = format!("{file_name}.css");
        fs::write(out_dir.join(&css_name), &combined)?;
        emitted.push((css_name, combined.len()));
    }

    println!("oj build (library): {} in {:?}", out_dir.display(), started.elapsed());
    emitted.sort_by(|a, b| b.1.cmp(&a.1));
    emitted.dedup();
    for (name, bytes) in &emitted {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    Ok(())
}

/// Map a lib format name to its (file extension, needs-a-global-name) pair.
fn lib_format(fmt: &str) -> Option<(&'static str, bool)> {
    match fmt {
        "es" | "esm" => Some(("js", false)),
        "cjs" => Some(("cjs", false)),
        "umd" => Some(("umd.js", true)),
        "iife" => Some(("iife.js", true)),
        _ => None,
    }
}

/// Normalize a public base path to a leading+trailing-slash form
/// (`"/"`, `"/app/"`). Empty/relative bases fall back to `"/"`.
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

/// Prefix an emitted asset filename (e.g. `assets/x-hash.js`) with the base.
fn with_base(filename: &str, base: &str) -> String {
    format!("{base}{}", filename.trim_start_matches('/'))
}

/// One entry in the Vite-compatible build manifest.
struct ManifestEntry {
    name: String,
    file: String,
    src: String,
    is_entry: bool,
    imports: Vec<String>,
    css: Vec<String>,
}

/// Build a Vite-compatible `manifest.json` value: keyed by root-relative
/// source path, each row carrying the emitted file plus name/isEntry/imports/
/// css. This is the exact shape Laravel/Rails/Django Vite plugins consume.
fn build_manifest(entries: &[ManifestEntry]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for e in entries {
        let mut row = serde_json::Map::new();
        row.insert("file".into(), e.file.clone().into());
        row.insert("name".into(), e.name.clone().into());
        row.insert("src".into(), e.src.clone().into());
        if e.is_entry {
            row.insert("isEntry".into(), true.into());
        }
        if !e.imports.is_empty() {
            row.insert("imports".into(), e.imports.clone().into());
        }
        if !e.css.is_empty() {
            row.insert("css".into(), e.css.clone().into());
        }
        map.insert(e.src.clone(), serde_json::Value::Object(row));
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_matches_vite_shape() {
        let m = build_manifest(&[ManifestEntry {
            name: "main".into(),
            file: "assets/main-abc123.js".into(),
            src: "src/main.tsx".into(),
            is_entry: true,
            imports: vec!["assets/vendor-def456.js".into()],
            css: vec!["assets/style-99.css".into()],
        }]);
        let row = &m["src/main.tsx"];
        assert_eq!(row["file"], "assets/main-abc123.js");
        assert_eq!(row["name"], "main");
        assert_eq!(row["src"], "src/main.tsx");
        assert_eq!(row["isEntry"], true);
        assert_eq!(row["imports"][0], "assets/vendor-def456.js");
        assert_eq!(row["css"][0], "assets/style-99.css");
    }

    #[test]
    fn non_entry_omits_isentry_and_empty_fields() {
        let m = build_manifest(&[ManifestEntry {
            name: "chunk".into(),
            file: "assets/chunk-1.js".into(),
            src: "chunk".into(),
            is_entry: false,
            imports: vec![],
            css: vec![],
        }]);
        let row = m["chunk"].as_object().unwrap();
        assert!(!row.contains_key("isEntry"));
        assert!(!row.contains_key("imports"));
        assert!(!row.contains_key("css"));
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
}
