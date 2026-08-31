// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use oj_resolver::OjResolver;

use crate::workerd::{render_config, resolve_fallback, Fallback, WorkerdOptions};

fn worker_conditions() -> Vec<String> {
    ["workerd", "worker", "browser", "module", "import", "default"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

struct FallbackState {
    root: PathBuf,
    resolver: OjResolver,
}

pub struct WorkerdSpawn {
    pub compat_date: String,
    pub compat_flags: Vec<String>,
    pub entry_specifier: String,
    pub vars: Vec<(String, String)>,
    pub service_bindings: Vec<(String, String)>,
}

const DEFAULT_COMPAT_DATE: &str = "2024-11-01";

impl WorkerdSpawn {
    pub fn from_wrangler(cfg: crate::wrangler::WranglerConfig, entry_specifier: String) -> Self {
        WorkerdSpawn {
            compat_date: cfg.compat_date.unwrap_or_else(|| DEFAULT_COMPAT_DATE.to_string()),
            compat_flags: cfg.compat_flags,
            entry_specifier,
            vars: cfg.vars,
            service_bindings: cfg.service_bindings,
        }
    }
}

pub struct WorkerdSession {
    child: Child,
    pub worker_addr: String,
}

impl WorkerdSession {
    pub fn worker_url(&self) -> String {
        format!("http://{}", self.worker_addr)
    }
}

impl Drop for WorkerdSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> std::io::Result<u16> {
    Ok(StdTcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

pub async fn spawn(
    root: &Path,
    workerd_bin: &Path,
    config_dir: &Path,
    opts: WorkerdSpawn,
) -> std::io::Result<WorkerdSession> {
    let fb = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let fb_port = fb.local_addr()?.port();
    let state = Arc::new(FallbackState {
        root: root.to_path_buf(),
        resolver: OjResolver::with_conditions(root, &worker_conditions()),
    });
    tokio::spawn(async move {
        let app = Router::new().route("/", get(fallback_handler)).with_state(state);
        let _ = axum::serve(fb, app).await;
    });

    let wd_port = free_port()?;

    let config = render_config(&WorkerdOptions {
        compat_date: opts.compat_date,
        compat_flags: opts.compat_flags,
        entry_specifier: opts.entry_specifier,
        fallback_addr: format!("127.0.0.1:{fb_port}"),
        socket_addr: format!("127.0.0.1:{wd_port}"),
        vars: opts.vars,
        service_bindings: opts.service_bindings,
    });
    std::fs::create_dir_all(config_dir)?;
    let config_path = config_dir.join("oj.workerd.capnp");
    std::fs::write(&config_path, config)?;

    let child = Command::new(workerd_bin)
        .arg("serve")
        .arg(&config_path)
        .arg("--experimental")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(WorkerdSession { child, worker_addr: format!("127.0.0.1:{wd_port}") })
}

async fn fallback_handler(
    State(state): State<Arc<FallbackState>>,
    Query(q): Query<Vec<(String, String)>>,
) -> Response {
    let get = |key: &str| q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let Some(specifier) = get("specifier") else {
        return (axum::http::StatusCode::BAD_REQUEST, "no specifier").into_response();
    };
    let raw = get("rawSpecifier").unwrap_or("");
    let referrer = get("referrer").unwrap_or("");
    match resolve_fallback(&state.root, &state.resolver, specifier, raw, referrer) {
        Fallback::Module { name, code } => {
            let body = serde_json::json!({ "name": name, "esModule": code }).to_string();
            ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        Fallback::Redirect { location } => (
            axum::http::StatusCode::MOVED_PERMANENTLY,
            [(axum::http::header::LOCATION, location)],
        )
            .into_response(),
        Fallback::NotFound => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wrangler::WranglerConfig;

    #[test]
    fn from_wrangler_carries_bindings_and_defaults_compat_date() {
        let cfg = WranglerConfig {
            compat_date: None,
            compat_flags: vec!["nodejs_compat".into()],
            vars: vec![("EVENTS_API_URL".into(), "https://x".into())],
            service_bindings: vec![("CONFIDENCE_RESOLVER".into(), "resolver".into())],
        };
        let spawn = WorkerdSpawn::from_wrangler(cfg, "/src/entry.tsx".into());
        assert_eq!(spawn.compat_date, DEFAULT_COMPAT_DATE);
        assert_eq!(spawn.compat_flags, vec!["nodejs_compat".to_string()]);
        assert_eq!(spawn.vars, vec![("EVENTS_API_URL".to_string(), "https://x".to_string())]);
        assert_eq!(
            spawn.service_bindings,
            vec![("CONFIDENCE_RESOLVER".to_string(), "resolver".to_string())]
        );
    }

    #[test]
    fn from_wrangler_keeps_an_explicit_compat_date() {
        let cfg = WranglerConfig { compat_date: Some("2026-08-01".into()), ..Default::default() };
        let spawn = WorkerdSpawn::from_wrangler(cfg, "/e".into());
        assert_eq!(spawn.compat_date, "2026-08-01");
    }
}
