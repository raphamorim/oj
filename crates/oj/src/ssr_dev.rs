// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_stream::wrappers::ReceiverStream;

struct Runner {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    _child: Child,
}

struct SsrState {
    client_url: Option<String>,
    proxy_prefixes: Vec<String>,
    root: PathBuf,
    runner: Arc<tokio::sync::Mutex<Runner>>,
}

pub async fn ssr_dev(
    root: PathBuf,
    entry: String,
    port: Option<u16>,
    host: Option<String>,
) -> anyhow::Result<()> {
    let built = oj_server::DevServer {
        root,
        port,
        bundle: false,
        host,
        config: None,
    }
    .build_app()
    .await?;

    let client_url =
        crate::build::derive_client_entry(&built.root, &entry).map(|rel| format!("/{rel}"));
    let base = format!("http://{}:{}", built.host, built.port);
    let entry_abs = built.root.join(&entry);
    let runner = spawn_runner(&built.root, &base, &entry_abs).await?;

    let ssr_state = Arc::new(SsrState {
        client_url: client_url.clone(),
        proxy_prefixes: built.proxy_prefixes.clone(),
        root: built.root.clone(),
        runner: Arc::new(tokio::sync::Mutex::new(runner)),
    });

    let app = built.router.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&ssr_state),
        ssr_route,
    ));

    let addr = std::net::SocketAddr::from((built.host, built.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
    println!("  {} dev (ssr + module runner)", oj_server::oj_brand());
    println!("  entry:  {entry}");
    match &client_url {
        Some(u) => println!("  client: {u} (hydration + hmr on)"),
        None => println!("  client: none (SSR only; add a *-client entry to hydrate)"),
    }
    let url = format!("http://localhost:{}/", built.port);
    println!("  {}", oj_server::link(&url, &oj_server::cell(&url)));
    axum::serve(listener, app).await?;
    Ok(())
}

async fn spawn_runner(root: &Path, base: &str, entry_abs: &Path) -> anyhow::Result<Runner> {
    let dir = oj_cache::cache_root(&root).join("ssr");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("runner.mjs");
    std::fs::write(&script, oj_server::SSR_RUNNER_JS)?;

    let mut cmd = tokio::process::Command::new("node");
    cmd.args([
        "--experimental-vm-modules",
        &script.to_string_lossy(),
        base,
        &entry_abs.to_string_lossy(),
    ])
    .current_dir(root)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit());
    if let Some(v8) = oj_server::node_compile_cache_opt_in(root) {
        cmd.env("NODE_COMPILE_CACHE", v8);
    }
    cmd.env("OJ_CACHE_ROOT", oj_cache::cache_root(&root));
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn SSR runner (node): {e}"))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(Runner {
        stdin,
        lines: BufReader::new(stdout).lines(),
        _child: child,
    })
}

enum Route {
    Document,
    Loader,
    Action { json: bool },
    Pass,
}

async fn ssr_route(State(state): State<Arc<SsrState>>, req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::POST && req.uri().path() == "/__oj_fn" {
        let bytes = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap_or_default();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
        let module_url = payload.get("module").and_then(|m| m.as_str()).unwrap_or("");
        let abs = state.root.join(module_url.trim_start_matches('/'));
        let cmd = serde_json::json!({
            "cmd": "call",
            "module": abs.to_string_lossy(),
            "name": payload.get("name").cloned().unwrap_or_default(),
            "args": payload.get("args").cloned().unwrap_or_else(|| serde_json::json!([])),
        });
        return match run_command(&state, cmd, "result").await {
            Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        };
    }
    match classify(&req, &state.proxy_prefixes) {
        Route::Document => render_route(&state, req.uri().path()).await,
        Route::Loader => data_response(
            run_command(
                &state,
                serde_json::json!({ "cmd": "load", "url": req.uri().path() }),
                "data",
            )
            .await,
        ),
        Route::Action { json } => {
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), 256 * 1024)
                .await
                .unwrap_or_default();
            let body = String::from_utf8_lossy(&bytes).into_owned();
            let cmd = serde_json::json!({ "cmd": "action", "url": path, "body": body });
            let data = run_command(&state, cmd, "data").await;
            if json {
                data_response(data)
            } else if data.is_ok() {
                (StatusCode::SEE_OTHER, [(header::LOCATION, path)]).into_response()
            } else {
                error_page(&data.unwrap_err())
            }
        }
        Route::Pass => next.run(req).await,
    }
}

fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    let path = req.uri().path();
    if path.starts_with("/@") || path.starts_with("/__") {
        return Route::Pass;
    }
    if path.rsplit('/').next().is_some_and(|seg| seg.contains('.')) {
        return Route::Pass;
    }
    if proxy_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
        return Route::Pass;
    }
    let is_loader = req.headers().contains_key("oj-loader");
    match *req.method() {
        Method::GET if is_loader => Route::Loader,
        Method::GET => Route::Document,
        Method::POST => Route::Action { json: is_loader },
        _ => Route::Pass,
    }
}

async fn run_command(
    state: &SsrState,
    cmd: serde_json::Value,
    reply_key: &str,
) -> Result<String, String> {
    let runner = Arc::clone(&state.runner);
    let reply_key = reply_key.to_owned();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut guard = runner.lock_owned().await;
        let result = async {
            guard
                .stdin
                .write_all(format!("{cmd}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            guard.stdin.flush().await.map_err(|e| e.to_string())?;
            match read_message(&mut guard.lines).await {
                Some(v) if v.get(&reply_key).and_then(|d| d.as_str()).is_some() => {
                    Ok(v[&reply_key].as_str().unwrap().to_owned())
                }
                Some(v) if v.get("error").and_then(|e| e.as_str()).is_some() => {
                    Err(v["error"].as_str().unwrap().to_owned())
                }
                _ => Err("SSR runner did not respond".to_string()),
            }
        }
        .await;
        let _ = tx.send(result);
    });
    rx.await
        .unwrap_or_else(|_| Err("SSR runner task cancelled".to_string()))
}

fn data_response(data: Result<String, String>) -> Response {
    match data {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn render_route(state: &SsrState, path: &str) -> Response {
    let mut guard = Arc::clone(&state.runner).lock_owned().await;
    let cmd = format!("{}\n", serde_json::json!({ "cmd": "render", "url": path }));
    if guard.stdin.write_all(cmd.as_bytes()).await.is_err() || guard.stdin.flush().await.is_err() {
        return error_page("SSR runner is not accepting input (did it crash?)");
    }
    let (data_json, head_html) = match read_message(&mut guard.lines).await {
        Some(v) if v.get("data").and_then(|d| d.as_str()).is_some() => (
            v["data"].as_str().unwrap().to_owned(),
            v.get("head")
                .and_then(|h| h.as_str())
                .unwrap_or("")
                .to_owned(),
        ),
        Some(v) if v.get("error").and_then(|e| e.as_str()).is_some() => {
            return error_page(v["error"].as_str().unwrap());
        }
        _ => ("null".to_string(), String::new()),
    };
    let v = match read_message(&mut guard.lines).await {
        Some(v) => v,
        None => return error_page("SSR runner did not respond"),
    };

    if let Some(html) = v.get("html").and_then(|h| h.as_str()) {
        return Html(page(state, html, &data_json, &head_html)).into_response();
    }
    if let Some(first_chunk) = v.get("chunk").and_then(|c| c.as_str()).map(str::to_owned) {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        let head = page_head(&data_json, &head_html);
        let tail = page_tail(state);
        tokio::spawn(async move {
            let mut guard = guard;
            let _ = tx.send(Ok(Bytes::from(head))).await;
            let _ = tx.send(Ok(Bytes::from(first_chunk))).await;
            while let Ok(Some(line)) = guard.lines.next_line().await {
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(v) if v.get("chunk").and_then(|c| c.as_str()).is_some() => {
                        let _ = tx
                            .send(Ok(Bytes::from(v["chunk"].as_str().unwrap().to_owned())))
                            .await;
                    }
                    Ok(v) if v.get("error").and_then(|e| e.as_str()).is_some() => {
                        let msg = html_escape(v["error"].as_str().unwrap());
                        let _ = tx
                            .send(Ok(Bytes::from(format!("<pre>[oj ssr] {msg}</pre>"))))
                            .await;
                        break;
                    }
                    _ => break,
                }
            }
            let _ = tx.send(Ok(Bytes::from(tail))).await;
        });
        return (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Body::from_stream(ReceiverStream::new(rx)),
        )
            .into_response();
    }
    error_page(
        v.get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown SSR error"),
    )
}

async fn read_message(lines: &mut Lines<BufReader<ChildStdout>>) -> Option<serde_json::Value> {
    let line = lines.next_line().await.ok().flatten()?;
    serde_json::from_str(&line).ok()
}

fn page_head(data_json: &str, head_html: &str) -> String {
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n{head_html}\n\
         <script>window.__OJ_DATA__={data_json}</script>\n\
         <script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
         <script type=\"module\" src=\"/@oj/client.js\"></script>\n</head>\n\
         <body><div id=\"app\">"
    )
}

fn page_tail(state: &SsrState) -> String {
    let entry = match &state.client_url {
        Some(u) => format!("<script type=\"module\" src=\"{u}\"></script>"),
        None => String::new(),
    };
    format!("</div>\n{entry}\n</body>\n</html>\n")
}

fn page(state: &SsrState, body: &str, data_json: &str, head_html: &str) -> String {
    format!(
        "{}{}{}",
        page_head(data_json, head_html),
        body,
        page_tail(state)
    )
}

fn error_page(msg: &str) -> Response {
    Html(format!(
        "<!doctype html><html><body><pre style=\"color:#c00;white-space:pre-wrap\">[oj ssr] {}</pre></body></html>",
        html_escape(msg)
    ))
    .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
