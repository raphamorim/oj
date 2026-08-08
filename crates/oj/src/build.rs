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
use rolldown::{BundlerBuilder, BundlerOptions, InputItem, RawMinifyOptions, SourceMapType};
use rolldown_plugin::{HookLoadArgs, HookLoadReturn, Plugin, SharedLoadPluginContext};

/// Rolldown dropped native CSS bundling (rolldown/rolldown#4271); like Vite,
/// the host tool owns CSS. This plugin loads `.css` imports as JS stubs
/// (CSS Modules export their scoped class map), collects the compiled CSS,
/// and `build()` emits one minified stylesheet linked from the HTML.
#[derive(Debug)]
struct OjCssPlugin {
    collected: Arc<Mutex<Vec<(String, String)>>>,
}

impl Plugin for OjCssPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:css")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Load
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let id = args.id.to_string();
        let collected = Arc::clone(&self.collected);
        async move {
            let path = id.split('?').next().unwrap_or(&id);
            if !path.ends_with(".css") {
                return Ok(None);
            }
            let source = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
            let output = oj_css::compile_css(path, &source, true)
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

pub async fn build(root: PathBuf, out: PathBuf) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;
    let html_path = root.join("index.html");
    let html = fs::read_to_string(&html_path)
        .with_context(|| format!("no index.html in {}", root.display()))?;

    let entries = module_script_srcs(&html);
    if entries.is_empty() {
        bail!("index.html has no <script type=\"module\" src=...> entry");
    }

    let out_dir = if out.is_absolute() { out } else { root.join(out) };
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
    let mut bundler = BundlerBuilder::default()
        .with_plugins(vec![Arc::new(OjCssPlugin { collected: Arc::clone(&collected_css) })])
        .with_options(BundlerOptions {
        input: Some(inputs),
        cwd: Some(root.clone()),
        dir: Some(out_dir.display().to_string()),
        entry_filenames: Some("assets/[name]-[hash].js".to_string().into()),
        chunk_filenames: Some("assets/[name]-[hash].js".to_string().into()),
        minify: Some(RawMinifyOptions::Bool(true)),
        sourcemap: Some(SourceMapType::File),
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
        .map_err(|errs| anyhow::anyhow!("build failed:\n{errs:?}"))?;

    for warning in &output.warnings {
        eprintln!("oj build warning: {warning:?}");
    }

    // Map each entry url to its hashed chunk filename for HTML rewriting.
    let mut rewritten_html = html.clone();
    let mut emitted: Vec<(String, usize)> = Vec::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            emitted.push((chunk.filename.to_string(), chunk.code.len()));
            if !chunk.is_entry {
                continue;
            }
            let Some(facade) = &chunk.facade_module_id else { continue };
            for entry in &entries {
                let entry_abs = root.join(entry.trim_start_matches('/'));
                if Path::new(facade.as_ref()) == entry_abs.as_path() {
                    rewritten_html =
                        rewritten_html.replace(entry.as_str(), &format!("/{}", chunk.filename));
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
            if source.contains("@import \"tailwindcss\"") || source.contains("@tailwind ") {
                // One-shot sidecar run with the app's own tailwind install.
                let script = out_dir.join(".tailwind-sidecar.mjs");
                fs::write(&script, include_str!("../../oj_server/src/assets/tailwind-sidecar.mjs"))?;
                let out = std::process::Command::new("node")
                    .args([script.to_str().unwrap(), "--once", src.to_str().unwrap(), root.to_str().unwrap()])
                    .output()
                    .context("node not found for tailwind build")?;
                let _ = fs::remove_file(&script);
                if !out.status.success() {
                    bail!("tailwind build failed: {}", String::from_utf8_lossy(&out.stderr));
                }
                let css = String::from_utf8_lossy(&out.stdout).into_owned();
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
        let link = format!("<link rel=\"stylesheet\" href=\"/{css_name}\" />");
        rewritten_html = match rewritten_html.find("</head>") {
            Some(idx) => format!("{}{}\n{}", &rewritten_html[..idx], link, &rewritten_html[idx..]),
            None => format!("{link}\n{rewritten_html}"),
        };
    }

    fs::write(out_dir.join("index.html"), rewritten_html)?;

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
