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
    pub define: Option<BTreeMap<String, serde_json::Value>>,
    pub env_prefix: Option<String>,
    pub env_dir: Option<String>,
    pub build: Option<BuildConfig>,
    pub preview: Option<PreviewConfig>,
    pub virtual_modules: Option<BTreeMap<String, String>>,
    pub bundle: Option<bool>,
    pub environments: Option<BTreeMap<String, serde_json::Value>>,
    pub optimize_deps: Option<OptimizeDepsConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OptimizeDepsConfig {
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub entries: Option<Vec<String>>,
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
    pub hmr_gate: Option<bool>,
    pub allowed_hosts: Option<Vec<String>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub proxy: Option<BTreeMap<String, ProxyEntry>>,
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
    pub conditions: Option<Vec<String>>,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibConfig {
    pub entry: String,
    pub name: Option<String>,
    pub formats: Option<Vec<String>>,
    pub file_name: Option<String>,
}
