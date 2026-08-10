// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The `oj.config` schema. Key names mirror Vite's so a user's mental model
//! (and much of their vite.config) ports directly. All fields optional: a
//! partial config and no-config-at-all both deserialize cleanly.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct OjConfig {
    pub root: Option<String>,
    pub base: Option<String>,
    pub public_dir: Option<String>,
    pub server: Option<ServerConfig>,
    pub resolve: Option<ResolveConfig>,
    /// Global constant replacements (`define`), values are JS expressions.
    pub define: Option<BTreeMap<String, serde_json::Value>>,
    pub env_prefix: Option<String>,
    pub env_dir: Option<String>,
    pub build: Option<BuildConfig>,
    pub preview: Option<PreviewConfig>,
    /// Virtual modules: import id -> module source. `import x from "virtual:id"`
    /// resolves here instead of the filesystem (the first slice of plugin
    /// support — resolve+load for author-provided modules).
    pub virtual_modules: Option<BTreeMap<String, String>>,
    /// oj-specific: default the dev server to registry bundle mode.
    pub bundle: Option<bool>,
    /// Vite Environment API: per-environment config overrides, keyed by name
    /// (e.g. `client`, `ssr`). Passed through to plugins so `applyToEnvironment`
    /// and `this.environment.config` see the merged per-environment config.
    pub environments: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewConfig {
    pub port: Option<u16>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub strict_port: Option<bool>,
    pub open: Option<bool>,
    pub cors: Option<bool>,
    pub allowed_hosts: Option<Vec<String>>,
    /// path prefix -> target (string) or detailed options.
    pub proxy: Option<BTreeMap<String, ProxyEntry>>,
}

/// `"/api": "http://localhost:3000"` or `"/api": { target, changeOrigin, ws, rewrite }`.
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
    /// Optional `^prefix` -> replacement rewrite of the request path.
    pub fn rewrite(&self) -> Option<(&str, &str)> {
        match self {
            ProxyEntry::Options(o) => o.rewrite.as_ref().map(|r| (r.from.as_str(), r.to.as_str())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyOptions {
    pub target: String,
    pub change_origin: Option<bool>,
    pub ws: Option<bool>,
    pub rewrite: Option<ProxyRewrite>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyRewrite {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolveConfig {
    /// find -> replacement (absolute or `./`-relative to root).
    pub alias: Option<BTreeMap<String, String>>,
    pub dedupe: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    /// Package `exports`/`imports` condition names (e.g. `["browser","import"]`).
    /// Per-environment overrides live under `environments.<name>.resolve`.
    pub conditions: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildConfig {
    pub out_dir: Option<String>,
    pub target: Option<String>,
    pub minify: Option<bool>,
    pub sourcemap: Option<bool>,
    /// Library mode: build a distributable library instead of an app.
    pub lib: Option<LibConfig>,
    /// SSR mode: build a Node-runnable server bundle from this entry.
    pub ssr: Option<String>,
    /// Prerender (SSG): route paths to render to static HTML at build time,
    /// e.g. `["/", "/about"]`. Requires an SSR entry.
    pub prerender: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LibConfig {
    /// Library entry module, relative to the app root.
    pub entry: String,
    /// Global/UMD export name (required for `umd`/`iife`).
    pub name: Option<String>,
    /// Output formats: any of `es`, `cjs`, `umd`, `iife`. Default `["es"]`.
    pub formats: Option<Vec<String>>,
    /// Output base filename (default: the entry's file stem).
    pub file_name: Option<String>,
}
