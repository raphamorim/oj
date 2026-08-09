// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Dev-server SSR with HMR (`oj dev --ssr <entry>`).
//!
//! The initial paint is server-rendered: on a full page load the server bundle
//! is (re)built and executed in Node to produce HTML. Hydration and hot module
//! replacement are then handled by oj's normal unbundled dev pipeline — the SSR
//! route is merged onto [`oj_server::DevServer`], so the client entry, module
//! graph, Fast Refresh, and the HMR WebSocket all come from it. Editing a
//! component hot-updates the running page with React state preserved and no
//! full reload, exactly like the non-SSR dev server. This is how Vite's dev SSR
//! works too: the server renders first paint, the dev server drives the client.
//!
//! CSS-module class names hash identically in the server bundle and the dev
//! pipeline (both key off the root-relative id), so the hydrated markup matches
//! the SSR HTML — no hydration mismatch.
//!
//! What's still coarse: the server bundle is rebuilt wholesale per full load
//! (Node per render), and there is no server-side module graph / SSR-side HMR —
//! an edit is reflected server-side only on the next full navigation. The
//! Environment API module runner is the larger remaining work.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Router,
    extract::State,
    response::{Html, IntoResponse, Response},
    routing::get,
};

struct SsrState {
    root: PathBuf,
    entry: String,
    server_dir: PathBuf,
    server_stem: String,
    /// URL of the client hydration entry (`/src/entry-client.tsx`), served by
    /// the dev pipeline. `None` => SSR-only, inert page.
    client_url: Option<String>,
    /// Serializes bundle rebuilds so a concurrent build can't wipe the `.mjs`
    /// out from under an in-flight Node render.
    build_lock: tokio::sync::Mutex<()>,
    version: AtomicU64,
}

pub async fn ssr_dev(root: PathBuf, entry: String, port: Option<u16>) -> anyhow::Result<()> {
    // Reuse the full dev server for the client side (Fast Refresh, HMR, module
    // compilation). We only add the server-rendered `/` route on top.
    let built = oj_server::DevServer { root, port, bundle: false }.build_app().await?;

    let server_stem = stem_of(&entry, "server");
    let client_url = derive_client_entry(&built.root, &entry).map(|rel| format!("/{rel}"));

    let ssr_state = Arc::new(SsrState {
        server_dir: built.root.join(".oj-cache").join("ssr").join("server"),
        server_stem,
        client_url: client_url.clone(),
        build_lock: tokio::sync::Mutex::new(()),
        version: AtomicU64::new(0),
        root: built.root.clone(),
        entry,
    });

    // The explicit `/` route overrides the dev server's index.html fallback;
    // every other path (modules, /@oj/*, /__ws, CSS, proxy) flows to the dev
    // router unchanged.
    let app = Router::new()
        .route("/", get(render))
        .with_state(Arc::clone(&ssr_state))
        .merge(built.router);

    let addr = std::net::SocketAddr::from((built.host, built.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind {addr}: {e}"))?;
    println!("  oj dev (ssr + hmr)");
    println!("  entry:  {}", ssr_state.entry);
    match &client_url {
        Some(u) => println!("  client: {u} (hydration + hmr on)"),
        None => println!("  client: none (SSR only; add a *-client entry to hydrate)"),
    }
    println!("  http://localhost:{}/", built.port);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn render(State(state): State<Arc<SsrState>>) -> Response {
    let _guard = state.build_lock.lock().await;
    if let Err(e) =
        crate::build::build_ssr(&state.root, &state.server_dir, &state.entry, false).await
    {
        return error_page(&format!("SSR build failed:\n{e}"));
    }
    let bundle = state.server_dir.join(format!("{}.mjs", state.server_stem));
    let v = state.version.fetch_add(1, Ordering::Relaxed);
    // Cache-bust the import so a rebuilt bundle is re-evaluated by Node.
    let script = format!(
        "import('file://{}?v={v}').then(m => process.stdout.write(String(m.render())))",
        bundle.display()
    );
    let out = tokio::process::Command::new("node")
        .args(["--input-type=module", "-e", &script])
        .current_dir(&state.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => {
            Html(page(&state, &String::from_utf8_lossy(&o.stdout))).into_response()
        }
        Ok(o) => error_page(&format!("SSR render failed:\n{}", String::from_utf8_lossy(&o.stderr))),
        Err(e) => error_page(&format!("could not spawn node: {e}")),
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

/// Stem of an entry path, falling back to `default` (e.g. "entry-server").
fn stem_of(entry: &str, default: &str) -> String {
    std::path::Path::new(entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(default)
        .to_string()
}

/// Derive the client hydration entry from the server entry by convention:
/// swap "server" -> "client" in the filename and return it (root-relative) if
/// that file exists, else None.
fn derive_client_entry(root: &std::path::Path, server_entry: &str) -> Option<String> {
    let file = std::path::Path::new(server_entry).file_name()?.to_str()?;
    if !file.contains("server") {
        return None;
    }
    let client_file = file.replace("server", "client");
    let client_rel = match std::path::Path::new(server_entry).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            format!("{}/{}", dir.to_string_lossy(), client_file)
        }
        _ => client_file,
    };
    root.join(&client_rel).is_file().then_some(client_rel)
}
