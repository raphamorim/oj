// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! End-to-end: oj generates a workerd config and answers workerd's module
//! fallback requests with TS/JSX-stripped ESM, so a route renders inside the
//! real Cloudflare runtime — no Node, Miniflare, or Vite plugin. Skips when the
//! `workerd` binary is not present.

use std::process::{Command, Stdio};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use oj_server::workerd::{find_workerd, render_config, WorkerdOptions};

#[tokio::test]
async fn native_workerd_renders_a_typescript_route() {
    let app = tempfile::tempdir().unwrap();
    let root = app.path().to_path_buf();

    let Some(workerd) = find_workerd(&root) else {
        eprintln!("SKIP workerd_native: workerd binary not found (set OJ_WORKERD_BIN)");
        return;
    };

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/dep.ts"),
        "export const greeting: string = \"rendered by real workerd\";\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/entry.tsx"),
        "import { greeting } from \"./dep.ts\";\n\
         interface Env {}\n\
         export default {\n\
           fetch(_req: Request, _env: Env): Response {\n\
             return new Response(greeting + \" [ts-stripped]\");\n\
           },\n\
         };\n",
    )
    .unwrap();

    // oj's module-fallback service: workerd asks it for each module.
    let fb_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fb_port = fb_listener.local_addr().unwrap().port();
    let fb_root = root.clone();
    let router = Router::new().route("/", get(fallback)).with_state(fb_root);
    tokio::spawn(async move { axum::serve(fb_listener, router).await.unwrap() });

    // a free port for the worker's HTTP socket
    let wd_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };

    let config = render_config(&WorkerdOptions {
        compat_date: "2026-08-01".into(),
        compat_flags: vec![],
        entry_specifier: "/src/entry.tsx".into(),
        fallback_addr: format!("127.0.0.1:{fb_port}"),
        socket_addr: format!("127.0.0.1:{wd_port}"),
        vars: vec![],
        service_bindings: vec![],
    });
    let config_path = root.join("oj.workerd.capnp");
    std::fs::write(&config_path, &config).unwrap();

    let mut child = Command::new(&workerd)
        .arg("serve")
        .arg(&config_path)
        .arg("--experimental")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn workerd");

    let mut body = String::new();
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if let Ok(resp) = client.get(format!("http://127.0.0.1:{wd_port}/")).send().await {
            if resp.status().is_success() {
                body = resp.text().await.unwrap_or_default();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let _ = child.kill();
    let out = child.wait_with_output().unwrap();

    assert!(
        body.contains("rendered by real workerd [ts-stripped]"),
        "workerd did not render the TS route.\n--- body ---\n{body}\n--- config ---\n{config}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
}

async fn fallback(State(root): State<std::path::PathBuf>, Query(q): Query<Vec<(String, String)>>) -> Response {
    let specifier = q.iter().find(|(k, _)| k == "specifier").map(|(_, v)| v.clone());
    let Some(specifier) = specifier else {
        return (axum::http::StatusCode::BAD_REQUEST, "no specifier").into_response();
    };
    match oj_server::workerd::fallback_module(&root, &specifier) {
        Some((name, code)) => {
            let body = serde_json::json!({ "name": name, "esModule": code }).to_string();
            ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response()
        }
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}
