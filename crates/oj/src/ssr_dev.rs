// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev-server SSR with a module runner (`oj dev --ssr <entry>`).
//!
//! The initial paint is server-rendered by a persistent Node **module runner**
//! (`oj_server::SSR_RUNNER_JS`): oj spawns it once, under the app root, and
//! drives it over stdin/stdout. The runner links the SSR module graph on demand
//! — app source is fetched already-compiled from the dev server's `/@ssr-*`
//! endpoints and evaluated with `vm.SourceTextModule`; node_modules import
//! natively. There is no Rolldown bundle and no per-render Node spawn, and an
//! edit re-evaluates only the changed modules and their importers (the runner
//! invalidates by file mtime).
//!
//! Hydration and client HMR are handled by oj's normal unbundled dev pipeline:
//! the SSR `/` route is merged onto [`oj_server::DevServer`], so the client
//! entry, Fast Refresh, and the HMR WebSocket all come from it. Editing a
//! component hot-updates the running page with React state preserved and no
//! reload; the server-rendered markup follows on the next full navigation.
//!
//! CSS-module class names hash identically in the runner and the dev pipeline
//! (both key off the root-relative id), so the hydrated markup matches the SSR
//! HTML — no hydration mismatch.

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_stream::wrappers::ReceiverStream;

/// A live handle to the spawned Node module runner.
struct Runner {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    _child: Child,
}

struct SsrState {
    /// URL of the client hydration entry (`/src/entry-client.tsx`), served by
    /// the dev pipeline. `None` => SSR-only, inert page.
    client_url: Option<String>,
    /// `server.proxy` prefixes, which must reach the proxy layer, not SSR.
    proxy_prefixes: Vec<String>,
    /// App root, to resolve a server-function module URL to its module id.
    root: PathBuf,
    runner: Arc<tokio::sync::Mutex<Runner>>,
}

pub async fn ssr_dev(root: PathBuf, entry: String, port: Option<u16>) -> anyhow::Result<()> {
    // Reuse the full dev server for the client side (Fast Refresh, HMR, module
    // compilation, and the /@ssr-* runner endpoints). We add the server-rendered
    // `/` route on top.
    let built = oj_server::DevServer { root, port, bundle: false }.build_app().await?;

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

    // A layer in front of the dev router: browser document navigations (any
    // path) are server-rendered per route; module/asset/proxy requests fall
    // through to the dev pipeline unchanged.
    let app = built
        .router
        .layer(axum::middleware::from_fn_with_state(Arc::clone(&ssr_state), ssr_route));

    let addr = std::net::SocketAddr::from((built.host, built.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
    println!("  oj dev (ssr + module runner)");
    println!("  entry:  {entry}");
    match &client_url {
        Some(u) => println!("  client: {u} (hydration + hmr on)"),
        None => println!("  client: none (SSR only; add a *-client entry to hydrate)"),
    }
    println!("  http://localhost:{}/", built.port);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Write the runner script under the app root (so Node resolves the app's
/// `node_modules`) and spawn it, piping stdin/stdout for the render protocol.
async fn spawn_runner(root: &Path, base: &str, entry_abs: &Path) -> anyhow::Result<Runner> {
    let dir = root.join(".oj-cache").join("ssr");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("runner.mjs");
    std::fs::write(&script, oj_server::SSR_RUNNER_JS)?;

    let mut child = tokio::process::Command::new("node")
        .args([
            "--experimental-vm-modules",
            &script.to_string_lossy(),
            base,
            &entry_abs.to_string_lossy(),
        ])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn SSR runner (node): {e}"))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(Runner { stdin, lines: BufReader::new(stdout).lines(), _child: child })
}

/// How an app-route request should be served.
enum Route {
    /// Browser navigation -> full HTML document.
    Document,
    /// Client data fetch (`oj-loader` header) -> JSON loader data.
    Loader,
    /// Form submit / mutation (POST) -> run the action, then revalidate.
    /// `json` = a fetch call wants JSON back; otherwise redirect (no-JS form).
    Action { json: bool },
    /// Not an app route (module, asset, `/@…`, `/__…`, proxy) -> dev pipeline.
    Pass,
}

/// Route app navigations/data/actions to the SSR runner; everything else
/// (modules, `/@oj/*`, `/@ssr-*`, assets, proxy, `/__ws`) to the dev pipeline.
async fn ssr_route(State(state): State<Arc<SsrState>>, req: Request, next: Next) -> Response {
    // Server function RPC: the client stub POSTs {module (url), name, args};
    // resolve the module to its id and run it on the SSR module runner.
    if req.method() == axum::http::Method::POST && req.uri().path() == "/__oj_fn" {
        let bytes = axum::body::to_bytes(req.into_body(), 4 * 1024 * 1024).await.unwrap_or_default();
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
        Route::Loader => {
            data_response(run_command(&state, serde_json::json!({ "cmd": "load", "url": req.uri().path() }), "data").await)
        }
        Route::Action { json } => {
            let path = req.uri().path().to_string();
            let bytes = axum::body::to_bytes(req.into_body(), 256 * 1024).await.unwrap_or_default();
            let body = String::from_utf8_lossy(&bytes).into_owned();
            let cmd = serde_json::json!({ "cmd": "action", "url": path, "body": body });
            let data = run_command(&state, cmd, "data").await;
            if json {
                data_response(data)
            } else if data.is_ok() {
                // Progressive enhancement: the mutation ran; redirect so the
                // browser re-GETs the updated document (POST/redirect/GET).
                (StatusCode::SEE_OTHER, [(header::LOCATION, path)]).into_response()
            } else {
                error_page(&data.unwrap_err())
            }
        }
        Route::Pass => next.run(req).await,
    }
}

/// Classify a request. App routes are extensionless paths that aren't a reserved
/// dev prefix (`/@…`, `/__…`) or a configured proxy prefix (`/api`). Modules and
/// assets carry extensions or the `/@` prefix; proxied paths reach the proxy
/// layer; everything else — `/`, `/about`, … — is ours.
fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    let path = req.uri().path();
    if path.starts_with("/@") || path.starts_with("/__") {
        return Route::Pass;
    }
    if path.rsplit('/').next().is_some_and(|seg| seg.contains('.')) {
        return Route::Pass; // has a file extension -> asset
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

/// Send a command to the runner and return the string under `reply_key`
/// (`"data"` for loaders/actions, `"result"` for server-function calls).
///
/// Runs in a detached task so a cancelled request (an aborted prefetch, say)
/// still drains the runner's one-line reply; reading inline would drop the lock
/// mid-protocol and leave a stale line the next render desyncs on.
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
            guard.stdin.write_all(format!("{cmd}\n").as_bytes()).await.map_err(|e| e.to_string())?;
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
    rx.await.unwrap_or_else(|_| Err("SSR runner task cancelled".to_string()))
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
    // The runner sends the loader's serialized data and the route's <head>
    // (title/meta) first, then the render output. Both go into the shell so the
    // client hydrates with the data (no refetch) and the head is SEO-visible.
    let (data_json, head_html) = match read_message(&mut guard.lines).await {
        Some(v) if v.get("data").and_then(|d| d.as_str()).is_some() => (
            v["data"].as_str().unwrap().to_owned(),
            v.get("head").and_then(|h| h.as_str()).unwrap_or("").to_owned(),
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

    // Buffered entry (render): one `{html}` message.
    if let Some(html) = v.get("html").and_then(|h| h.as_str()) {
        return Html(page(state, html, &data_json, &head_html)).into_response();
    }
    // Streaming entry (renderStream): `{chunk}`… then `{end}`. Flush the shell
    // immediately, forward each chunk as React produces it, close with the tail.
    if let Some(first_chunk) = v.get("chunk").and_then(|c| c.as_str()).map(str::to_owned) {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        let head = page_head(&data_json, &head_html);
        let tail = page_tail(state);
        tokio::spawn(async move {
            let mut guard = guard; // hold the runner lock for the whole stream
            let _ = tx.send(Ok(Bytes::from(head))).await;
            let _ = tx.send(Ok(Bytes::from(first_chunk))).await;
            while let Ok(Some(line)) = guard.lines.next_line().await {
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(v) if v.get("chunk").and_then(|c| c.as_str()).is_some() => {
                        let _ = tx.send(Ok(Bytes::from(v["chunk"].as_str().unwrap().to_owned()))).await;
                    }
                    Ok(v) if v.get("error").and_then(|e| e.as_str()).is_some() => {
                        let msg = html_escape(v["error"].as_str().unwrap());
                        let _ = tx.send(Ok(Bytes::from(format!("<pre>[oj ssr] {msg}</pre>")))).await;
                        break;
                    }
                    _ => break, // {end} or anything else closes the stream
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
    error_page(v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown SSR error"))
}

/// Read and parse one JSON-lines message from the runner (None if it closed or
/// sent malformed output).
async fn read_message(
    lines: &mut Lines<BufReader<ChildStdout>>,
) -> Option<serde_json::Value> {
    let line = lines.next_line().await.ok().flatten()?;
    serde_json::from_str(&line).ok()
}

/// The document head + open of `#app`: charset, the route loader's data (so the
/// client hydrates without refetching), then the Fast Refresh preamble (must be
/// the first module script, so the refresh hook installs before any module
/// pulls in React) and the dev HMR client.
fn page_head(data_json: &str, head_html: &str) -> String {
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n{head_html}\n\
         <script>window.__OJ_DATA__={data_json}</script>\n\
         <script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
         <script type=\"module\" src=\"/@oj/client.js\"></script>\n</head>\n\
         <body><div id=\"app\">"
    )
}

/// Close `#app`, then the hydration entry (served by the dev router), then the
/// document.
fn page_tail(state: &SsrState) -> String {
    let entry = match &state.client_url {
        Some(u) => format!("<script type=\"module\" src=\"{u}\"></script>"),
        None => String::new(),
    };
    format!("</div>\n{entry}\n</body>\n</html>\n")
}

/// The full document for a buffered render.
fn page(state: &SsrState, body: &str, data_json: &str, head_html: &str) -> String {
    format!("{}{}{}", page_head(data_json, head_html), body, page_tail(state))
}

fn error_page(msg: &str) -> Response {
    Html(format!(
        "<!doctype html><html><body><pre style=\"color:#c00;white-space:pre-wrap\">[oj ssr] {}</pre></body></html>",
        html_escape(msg)
    ))
    .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
