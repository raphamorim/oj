// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::net::TcpListener as StdTcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::workerd::{fallback_module, render_config, WorkerdOptions};

pub struct WorkerdSpawn {
    pub compat_date: String,
    pub compat_flags: Vec<String>,
    pub entry_specifier: String,
    pub vars: Vec<(String, String)>,
    pub service_bindings: Vec<(String, String)>,
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

/// Boots a workerd process for `root`: starts oj's module-fallback service, picks
/// a port for the worker's HTTP socket, writes the config into `config_dir`, and
/// spawns `workerd serve`. The returned session kills workerd on drop.
pub async fn spawn(
    root: &Path,
    workerd_bin: &Path,
    config_dir: &Path,
    opts: WorkerdSpawn,
) -> std::io::Result<WorkerdSession> {
    let fb = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let fb_port = fb.local_addr()?.port();
    let fb_root = root.to_path_buf();
    tokio::spawn(async move {
        let app = Router::new().route("/", get(fallback_handler)).with_state(fb_root);
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
    State(root): State<PathBuf>,
    Query(q): Query<Vec<(String, String)>>,
) -> Response {
    let Some((_, specifier)) = q.iter().find(|(k, _)| k == "specifier") else {
        return (axum::http::StatusCode::BAD_REQUEST, "no specifier").into_response();
    };
    match fallback_module(&root, specifier) {
        Some((name, code)) => {
            let body = serde_json::json!({ "name": name, "esModule": code }).to_string();
            ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}
