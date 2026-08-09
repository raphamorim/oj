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
    Router,
    extract::State,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

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
    runner: tokio::sync::Mutex<Runner>,
}

pub async fn ssr_dev(root: PathBuf, entry: String, port: Option<u16>) -> anyhow::Result<()> {
    // Reuse the full dev server for the client side (Fast Refresh, HMR, module
    // compilation, and the /@ssr-* runner endpoints). We add the server-rendered
    // `/` route on top.
    let built = oj_server::DevServer { root, port, bundle: false }.build_app().await?;

    let client_url = derive_client_entry(&built.root, &entry).map(|rel| format!("/{rel}"));
    let base = format!("http://{}:{}", built.host, built.port);
    let entry_abs = built.root.join(&entry);
    let runner = spawn_runner(&built.root, &base, &entry_abs).await?;

    let ssr_state = Arc::new(SsrState {
        client_url: client_url.clone(),
        runner: tokio::sync::Mutex::new(runner),
    });

    // The explicit `/` route overrides the dev server's index.html fallback;
    // every other path flows to the dev router unchanged.
    let app = Router::new()
        .route("/", get(render))
        .with_state(Arc::clone(&ssr_state))
        .merge(built.router);

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

async fn render(State(state): State<Arc<SsrState>>) -> Response {
    let mut runner = state.runner.lock().await;
    if runner.stdin.write_all(b"{\"cmd\":\"render\"}\n").await.is_err()
        || runner.stdin.flush().await.is_err()
    {
        return error_page("SSR runner is not accepting input (did it crash?)");
    }
    match runner.lines.next_line().await {
        Ok(Some(line)) => match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) if v.get("html").and_then(|h| h.as_str()).is_some() => {
                Html(page(&state, v["html"].as_str().unwrap())).into_response()
            }
            Ok(v) => error_page(v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown SSR error")),
            Err(_) => error_page(&format!("SSR runner sent malformed output:\n{line}")),
        },
        _ => error_page("SSR runner did not respond"),
    }
}

/// Wrap the server-rendered `body` in an HTML document. The Fast Refresh
/// preamble must be the first module script (it installs the React refresh
/// hook before any module pulls in React); the dev HMR client comes next, then
/// the hydration entry — all served by the merged dev router.
fn page(state: &SsrState, body: &str) -> String {
    let head = "<script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
                <script type=\"module\" src=\"/@oj/client.js\"></script>";
    let entry = match &state.client_url {
        Some(u) => format!("<script type=\"module\" src=\"{u}\"></script>"),
        None => String::new(),
    };
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n{head}\n</head>\n\
         <body><div id=\"app\">{body}</div>\n{entry}\n</body>\n</html>\n"
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
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Derive the client hydration entry from the server entry by convention:
/// swap "server" -> "client" in the filename and return it (root-relative) if
/// that file exists, else None.
fn derive_client_entry(root: &Path, server_entry: &str) -> Option<String> {
    let file = Path::new(server_entry).file_name()?.to_str()?;
    if !file.contains("server") {
        return None;
    }
    let client_file = file.replace("server", "client");
    let client_rel = match Path::new(server_entry).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            format!("{}/{}", dir.to_string_lossy(), client_file)
        }
        _ => client_file,
    };
    root.join(&client_rel).is_file().then_some(client_rel)
}
