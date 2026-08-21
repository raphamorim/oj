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

fn server_fn_module(root: &Path, module_url: &str) -> Option<PathBuf> {
    const SERVER_SUFFIXES: [&str; 8] = [
        ".server.ts",
        ".server.tsx",
        ".server.js",
        ".server.jsx",
        ".server.mts",
        ".server.mjs",
        ".server.cts",
        ".server.cjs",
    ];

    let rel = module_url.split('?').next().unwrap_or(module_url);
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    let candidate = normalize_path(&root.join(rel));
    if !candidate.starts_with(normalize_path(root)) {
        return None;
    }
    let name = candidate.file_name().and_then(|n| n.to_str())?;
    if !SERVER_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return None;
    }
    if !candidate.is_file() {
        return None;
    }
    Some(candidate)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

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
        let Some(abs) = server_fn_module(&state.root, module_url) else {
            return (
                StatusCode::FORBIDDEN,
                format!("oj: not a server module: {module_url}"),
            )
                .into_response();
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A project with one server module and one ordinary module, plus a sibling
    /// directory outside it.
    fn fixture() -> (PathBuf, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("oj-serverfn-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("app");
        std::fs::create_dir_all(root.join("src/api")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        for name in [
            "src/greeting.server.ts",
            "src/greeting.server.tsx",
            "src/api/user.server.js",
            "src/api/user.server.mjs",
            "src/App.tsx",
            "src/main.tsx",
            "src/server.ts",
            "src/serverish.ts",
            "src/greeting.server",
            "src/greeting.server.css",
            "package.json",
        ] {
            std::fs::write(root.join(name), "x").unwrap();
        }
        std::fs::write(base.join("outside/secrets.server.ts"), "x").unwrap();
        (base, root)
    }

    #[test]
    fn a_server_function_call_names_a_server_module_inside_the_project() {
        let (base, root) = fixture();
        for ok in [
            "src/greeting.server.ts",
            "/src/greeting.server.ts",
            "src/greeting.server.tsx",
            "src/api/user.server.js",
            "src/api/user.server.mjs",
            "src/greeting.server.ts?t=123",
            // `..` that lands back inside is still inside.
            "src/../src/greeting.server.ts",
        ] {
            let resolved = server_fn_module(&root, ok).unwrap_or_else(|| panic!("{ok} rejected"));
            assert!(resolved.starts_with(&root), "{ok} -> {resolved:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_server_function_call_cannot_name_a_module_outside_the_project() {
        let (base, root) = fixture();
        // The `module` field comes off the request body, and a browser can POST
        // here cross-origin without a preflight. Unchecked, this is "import and
        // run any module on the machine" from any page the developer has open.
        for hostile in [
            "../outside/secrets.server.ts",
            "../../../../etc/evil.server.js",
            "src/../../outside/secrets.server.ts",
            "/etc/passwd",
            "/Users/someone/.ssh/id_rsa",
            "..",
            "/",
            "",
            " ",
        ] {
            assert!(
                server_fn_module(&root, hostile).is_none(),
                "{hostile:?} accepted"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_server_function_call_cannot_name_an_ordinary_module() {
        let (base, root) = fixture();
        // Production dispatches through a table built from `**/*.server.*`; dev
        // must not be able to reach further than that.
        for not_a_server_module in [
            "src/App.tsx",
            "src/main.tsx",
            "package.json",
            "src/server.ts",
            "src/greeting.server",
            "src/greeting.server.css",
            "src/serverish.ts",
            "node_modules/react/index.js",
        ] {
            assert!(
                server_fn_module(&root, not_a_server_module).is_none(),
                "{not_a_server_module:?} accepted"
            );
        }
        // A server module that does not exist is refused too.
        assert!(server_fn_module(&root, "src/missing.server.ts").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_normalization_does_not_touch_the_filesystem() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
        assert_eq!(normalize_path(Path::new("/a/../..")), PathBuf::from("/"));
        assert_eq!(normalize_path(Path::new("a/b/../b")), PathBuf::from("a/b"));
    }

    #[test]
    fn requests_for_assets_and_internals_are_passed_through() {
        let prefixes: Vec<String> = vec!["/api".to_string()];
        let route = |method: &str, path: &str| {
            let req = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            matches!(classify(&req, &prefixes), Route::Pass)
        };
        assert!(route("GET", "/@oj/client.js"), "internal urls pass through");
        assert!(route("GET", "/__oj_fn"), "double-underscore urls pass through");
        assert!(route("GET", "/src/App.tsx"), "a file url passes through");
        assert!(route("GET", "/api/users"), "a proxied prefix passes through");
        assert!(route("PUT", "/dashboard"), "an unhandled method passes through");
        assert!(!route("GET", "/dashboard"), "a route is a document");
        assert!(!route("POST", "/dashboard"), "a post is an action");
    }
}
