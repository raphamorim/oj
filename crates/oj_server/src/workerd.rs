// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

pub fn platform_tag() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-64",
        ("linux", "x86_64") => "linux-64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", _) => "windows-64",
        _ => "unknown",
    }
}

pub fn find_workerd(root: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OJ_WORKERD_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let tag = platform_tag();
    let bin = format!("node_modules/@cloudflare/workerd-{tag}/bin/workerd");
    let pnpm_prefix = format!("@cloudflare+workerd-{tag}@");
    let mut dir = Some(root);
    while let Some(d) = dir {
        let direct = d.join(&bin);
        if direct.is_file() {
            return Some(direct);
        }
        if let Ok(entries) = std::fs::read_dir(d.join("node_modules/.pnpm")) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&pnpm_prefix) {
                    let cand = e.path().join(&bin);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
        }
        dir = d.parent();
    }
    None
}

pub struct WorkerdOptions {
    pub compat_date: String,
    pub compat_flags: Vec<String>,
    pub entry_specifier: String,
    pub fallback_addr: String,
    pub socket_addr: String,
    pub vars: Vec<(String, String)>,
    pub service_bindings: Vec<(String, String)>,
}

fn capnp_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn render_config(o: &WorkerdOptions) -> String {
    let stub = format!(
        "import handler from {};\nexport default handler;",
        json_ident(&o.entry_specifier)
    );
    let flags = o
        .compat_flags
        .iter()
        .map(|f| capnp_str(f))
        .collect::<Vec<_>>()
        .join(", ");
    let mut bindings: Vec<String> = Vec::new();
    for (k, v) in &o.vars {
        bindings.push(format!("    (name = {}, text = {}),", capnp_str(k), capnp_str(v)));
    }
    for (k, service) in &o.service_bindings {
        bindings.push(format!(
            "    (name = {}, service = (name = {})),",
            capnp_str(k),
            capnp_str(service)
        ));
    }
    let bindings = bindings.join("\n");
    format!(
        "using Workerd = import \"/workerd/workerd.capnp\";\n\
const config :Workerd.Config = (\n  \
  services = [ (name = \"main\", worker = .mainWorker) ],\n  \
  sockets = [ (name = \"http\", address = {socket}, http = (), service = \"main\") ],\n\
);\n\
const mainWorker :Workerd.Worker = (\n  \
  compatibilityDate = {date},\n  \
  compatibilityFlags = [{flags}],\n  \
  modules = [ (name = \"entry.js\", esModule = {stub}) ],\n  \
  moduleFallback = {fallback},\n  \
  bindings = [\n{bindings}\n  ],\n\
);\n",
        socket = capnp_str(&o.socket_addr),
        date = capnp_str(&o.compat_date),
        flags = flags,
        stub = capnp_str(&stub),
        fallback = capnp_str(&o.fallback_addr),
        bindings = bindings,
    )
}

fn json_ident(spec: &str) -> String {
    serde_json::to_string(spec).unwrap_or_else(|_| format!("{spec:?}"))
}

const RESOLVE_EXTS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs"];

fn resolve_file(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    for ext in RESOLVE_EXTS {
        let p = PathBuf::from(format!("{}{ext}", base.display()));
        if p.is_file() {
            return Some(p);
        }
    }
    if base.is_dir() {
        for ext in RESOLVE_EXTS {
            let p = base.join(format!("index{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Answers a workerd module-fallback request: given the resolved (root-relative)
/// specifier, returns the module `name` (specifier without a leading slash) and
/// its ESM source, transpiled from TS/JSX. `None` means 404.
pub fn fallback_module(root: &Path, specifier: &str) -> Option<(String, String)> {
    let name = specifier.trim_start_matches('/').to_string();
    let file = resolve_file(&root.join(&name))?;
    let source = std::fs::read_to_string(&file).ok()?;
    let out =
        oj_compiler::compile(&file, &source, &oj_compiler::CompileOptions::prod()).ok()?;
    Some((name, out.code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_has_socket_fallback_bindings_and_stub() {
        let cfg = render_config(&WorkerdOptions {
            compat_date: "2026-08-01".into(),
            compat_flags: vec!["nodejs_compat".into()],
            entry_specifier: "/src/entry.tsx".into(),
            fallback_addr: "127.0.0.1:8899".into(),
            socket_addr: "127.0.0.1:0".into(),
            vars: vec![("EVENTS_API_URL".into(), "https://x".into())],
            service_bindings: vec![("CONFIDENCE_RESOLVER".into(), "resolver".into())],
        });
        assert!(cfg.contains(r#"moduleFallback = "127.0.0.1:8899""#), "{cfg}");
        assert!(cfg.contains(r#"address = "127.0.0.1:0""#), "{cfg}");
        assert!(cfg.contains(r#"compatibilityFlags = ["nodejs_compat"]"#), "{cfg}");
        assert!(cfg.contains(r#"(name = "EVENTS_API_URL", text = "https://x")"#), "{cfg}");
        assert!(
            cfg.contains(r#"(name = "CONFIDENCE_RESOLVER", service = (name = "resolver"))"#),
            "{cfg}"
        );
        // the embedded entry stub imports the real entry via fallback
        assert!(cfg.contains(r#"import handler from \"/src/entry.tsx\""#), "{cfg}");
    }

    #[test]
    fn fallback_module_strips_types_and_names_without_slash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/dep.ts"),
            "export const hi: string = \"x\";\n",
        )
        .unwrap();
        let (name, code) = fallback_module(dir.path(), "/src/dep.ts").unwrap();
        assert_eq!(name, "src/dep.ts", "name must drop the leading slash");
        assert!(code.contains("export const hi"), "{code}");
        assert!(!code.contains(": string"), "TS type survived: {code}");
    }

    #[test]
    fn fallback_module_404s_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fallback_module(dir.path(), "/nope.ts").is_none());
    }
}
