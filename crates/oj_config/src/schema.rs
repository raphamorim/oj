// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OjConfig {
    pub root: Option<String>,
    pub base: Option<String>,
    pub public_dir: Option<String>,
    pub server: Option<ServerConfig>,
    pub resolve: Option<ResolveConfig>,
    pub css: Option<CssConfig>,
    pub define: Option<BTreeMap<String, serde_json::Value>>,
    pub env_prefix: Option<StringOrList>,
    pub env_dir: Option<String>,
    pub build: Option<BuildConfig>,
    pub preview: Option<PreviewConfig>,
    pub virtual_modules: Option<BTreeMap<String, String>>,
    pub bundle: Option<bool>,
    pub environments: Option<BTreeMap<String, serde_json::Value>>,
    pub optimize_deps: Option<OptimizeDepsConfig>,
    /// Vite's `oxc` block (`oxc.jsx.{runtime,importSource,pragma,pragmaFrag}`);
    /// kept opaque because Vite also admits `oxc: false`. See `jsx_settings`.
    pub oxc: Option<serde_json::Value>,
    /// Vite <=7's `esbuild` block (`jsx`, `jsxImportSource`, `jsxFactory`,
    /// `jsxFragment`); opaque for the same reason. See `jsx_settings`.
    pub esbuild: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s.clone()],
            StringOrList::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CssConfig {
    pub preprocessor_options: Option<BTreeMap<String, PreprocessorEntry>>,
    /// Vite's `css.devSourcemap`: inline source maps on dev-served CSS.
    pub dev_sourcemap: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreprocessorEntry {
    pub additional_data: Option<String>,
    /// Every other option, forwarded verbatim: Sass `loadPaths`/`includePaths`,
    /// Less `paths`/`javascriptEnabled`/`globalVars`/`modifyVars`, Stylus
    /// `paths`/`define`, ... (Vite passes preprocessorOptions straight through).
    #[serde(flatten)]
    pub rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OptimizeDepsConfig {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub entries: Option<Vec<String>>,
    /// Deps whose CommonJS exports the scanner can't see statically; forces
    /// CJS->ESM interop for them.
    pub needs_interop: Option<Vec<String>>,
    /// Ignore any cached pre-bundle and rebuild from scratch.
    pub force: Option<bool>,
    /// Opaque options forwarded to the underlying bundler (esbuild on Vite <=7).
    pub esbuild_options: Option<serde_json::Value>,
    /// Opaque options forwarded to the underlying bundler (rolldown on Vite 8).
    pub rolldown_options: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreviewConfig {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub strict_port: Option<bool>,
    pub open: Option<bool>,
    pub cors: Option<bool>,
    pub hmr: Option<HmrConfig>,
    pub hmr_gate: Option<bool>,
    pub allowed_hosts: Option<Vec<String>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub proxy: Option<BTreeMap<String, ProxyEntry>>,
    pub fs: Option<FsConfig>,
    pub warmup: Option<WarmupConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum HmrConfig {
    Toggle(bool),
    Options(HmrOptions),
}

impl HmrConfig {
    pub fn is_disabled(&self) -> bool {
        matches!(self, HmrConfig::Toggle(false))
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HmrOptions {
    pub path: Option<String>,
    pub port: Option<u16>,
    pub overlay: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WarmupConfig {
    /// Client modules to transform eagerly at startup so their first request is warm.
    pub client_files: Option<Vec<String>>,
    pub ssr_files: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FsConfig {
    pub allow: Option<Vec<String>>,
    pub strict: Option<bool>,
    pub deny: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProxyEntry {
    Target(String),
    Options(ProxyOptions),
}

impl ProxyEntry {
    pub fn target(&self) -> &str {
        match self {
            ProxyEntry::Target(t) => t,
            ProxyEntry::Options(o) => &o.target,
        }
    }
    pub fn change_origin(&self) -> bool {
        matches!(self, ProxyEntry::Options(o) if o.change_origin.unwrap_or(false))
    }
    pub fn ws(&self) -> bool {
        matches!(self, ProxyEntry::Options(o) if o.ws.unwrap_or(false))
    }
    pub fn rewrite(&self) -> Option<(&str, &str)> {
        match self {
            ProxyEntry::Options(o) => o.rewrite.as_ref().map(|r| (r.from.as_str(), r.to.as_str())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyOptions {
    pub target: String,
    pub change_origin: Option<bool>,
    pub ws: Option<bool>,
    pub rewrite: Option<ProxyRewrite>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyRewrite {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResolveConfig {
    pub alias: Option<BTreeMap<String, String>>,
    pub dedupe: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub main_fields: Option<Vec<String>>,
    pub conditions: Option<Vec<String>>,
    pub preserve_symlinks: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BuildConfig {
    pub out_dir: Option<String>,
    pub target: Option<String>,
    pub minify: Option<bool>,
    pub sourcemap: Option<bool>,
    pub lib: Option<LibConfig>,
    pub ssr: Option<String>,
    pub prerender: Option<Vec<String>>,
    pub rollup_options: Option<serde_json::Value>,
    pub rolldown_options: Option<serde_json::Value>,
    pub assets_inline_limit: Option<u64>,
    pub css_code_split: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibConfig {
    pub entry: String,
    pub name: Option<String>,
    pub formats: Option<Vec<String>>,
    pub file_name: Option<String>,
}
