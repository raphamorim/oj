// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

use oj_resolver::OjResolver;

pub fn is_cloudflare_app(root: &Path) -> bool {
    ["wrangler.jsonc", "wrangler.json", "wrangler.toml"]
        .iter()
        .any(|f| root.join(f).is_file())
}

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

fn spec_to_file(root: &Path, specifier: &str) -> Option<PathBuf> {
    let as_abs = PathBuf::from(specifier);
    if as_abs.is_absolute() {
        if let Some(f) = resolve_file(&as_abs) {
            return Some(f);
        }
    }
    resolve_file(&root.join(specifier.trim_start_matches('/')))
}

pub fn fallback_module(
    root: &Path,
    resolver: &OjResolver,
    specifier: &str,
) -> Option<(String, String)> {
    let file = spec_to_file(root, specifier)?;
    serve_resolved(&file, specifier, resolver)
}

fn serve_resolved(
    file: &Path,
    specifier: &str,
    resolver: &OjResolver,
) -> Option<(String, String)> {
    let source = std::fs::read_to_string(file).ok()?;
    let name = specifier.trim_start_matches('/').to_string();
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "json" {
        return Some((name, format!("export default {source};\n")));
    }
    let is_cjs = match ext {
        "cjs" => true,
        "js" => !oj_compiler::cjs::has_module_syntax_pub(file, &source),
        _ => false,
    };
    if is_cjs {
        let dir = file.parent()?.to_path_buf();
        let url = file.to_string_lossy().into_owned();
        let mut resolve = |spec: &str| -> Option<String> {
            if crate::is_node_builtin(spec) {
                return Some(format!("node:{}", spec.strip_prefix("node:").unwrap_or(spec)));
            }
            resolver.resolve(&dir, spec).ok().map(|p| p.to_string_lossy().into_owned())
        };
        let out = oj_compiler::cjs::wrap_cjs(file, &url, &source, &mut resolve).ok()?;
        return Some((name, out.code));
    }
    let out = oj_compiler::compile(file, &source, &oj_compiler::CompileOptions::prod()).ok()?;
    Some((name, out.code))
}

pub enum Fallback {
    Module { name: String, code: String },
    Redirect { location: String },
    NotFound,
}

fn rewrite_hash_aliases(code: &mut String, aliases: &[(String, PathBuf)]) {
    for (key, target) in aliases {
        if !key.starts_with('#') {
            continue;
        }
        let Some(file) = resolve_file(target) else {
            continue;
        };
        let abs = file.to_string_lossy();
        for quote in ['"', '\''] {
            let from = format!("{quote}{key}{quote}");
            if code.contains(&from) {
                *code = code.replace(&from, &format!("{quote}{abs}{quote}"));
            }
        }
    }
}

pub fn resolve_fallback(
    root: &Path,
    resolver: &OjResolver,
    aliases: &[(String, PathBuf)],
    specifier: &str,
    raw_specifier: &str,
    referrer: &str,
) -> Fallback {
    if let Some(file) = spec_to_file(root, specifier) {
        let resolved_index = file.file_stem().and_then(|s| s.to_str()) == Some("index");
        let spec_index = Path::new(specifier).file_stem().and_then(|s| s.to_str()) == Some("index");
        if resolved_index && !spec_index {
            return Fallback::Redirect { location: file.to_string_lossy().into_owned() };
        }
        if let Some((name, mut code)) = serve_resolved(&file, specifier, resolver) {
            rewrite_hash_aliases(&mut code, aliases);
            return Fallback::Module { name, code };
        }
        return Fallback::NotFound;
    }
    let key = if raw_specifier.is_empty() {
        specifier.trim_start_matches('/')
    } else {
        raw_specifier
    };
    if let Some((_, target)) = aliases.iter().find(|(k, _)| k == key) {
        if let Some(file) = resolve_file(target) {
            return Fallback::Redirect { location: file.to_string_lossy().into_owned() };
        }
    }
    let importer_dir = spec_to_file(root, referrer)
        .and_then(|f| f.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf());
    if raw_specifier.starts_with('#') {
        if let Some(file) = resolve_hash_import(&importer_dir, raw_specifier) {
            return Fallback::Redirect { location: file.to_string_lossy().into_owned() };
        }
    }
    if !raw_specifier.is_empty()
        && !raw_specifier.starts_with('.')
        && !raw_specifier.starts_with('/')
    {
        if let Ok(abs) = resolver.resolve(&importer_dir, raw_specifier) {
            return Fallback::Redirect { location: abs.to_string_lossy().into_owned() };
        }
    }
    Fallback::NotFound
}

fn resolve_hash_import(importer_dir: &Path, raw: &str) -> Option<PathBuf> {
    let mut dir = Some(importer_dir);
    while let Some(d) = dir {
        let pkg = d.join("package.json");
        if pkg.is_file() {
            let text = std::fs::read_to_string(&pkg).ok()?;
            let v: serde_json::Value = serde_json::from_str(&text).ok()?;
            let imports = v.get("imports").and_then(|i| i.as_object());
            if let Some(imports) = imports {
                if let Some(target) = match_imports(imports, raw) {
                    let joined = d.join(target.trim_start_matches("./"));
                    if let Some(f) = resolve_file(&joined) {
                        return Some(f);
                    }
                }
                return None;
            }
        }
        dir = d.parent();
    }
    None
}

fn match_imports(imports: &serde_json::Map<String, serde_json::Value>, raw: &str) -> Option<String> {
    if let Some(v) = imports.get(raw).and_then(|x| x.as_str()) {
        return Some(v.to_string());
    }
    for (k, v) in imports {
        if let Some(prefix) = k.strip_suffix('*') {
            if let Some(rest) = raw.strip_prefix(prefix) {
                if let Some(tmpl) = v.as_str() {
                    return Some(tmpl.replace('*', rest));
                }
            }
        }
    }
    None
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
        let (name, code) = fallback_module(dir.path(), &resolver(dir.path()), "/src/dep.ts").unwrap();
        assert_eq!(name, "src/dep.ts", "name must drop the leading slash");
        assert!(code.contains("export const hi"), "{code}");
        assert!(!code.contains(": string"), "TS type survived: {code}");
    }

    #[test]
    fn detects_cloudflare_app_by_wrangler_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_cloudflare_app(dir.path()));
        std::fs::write(dir.path().join("wrangler.jsonc"), "{}").unwrap();
        assert!(is_cloudflare_app(dir.path()));
    }

    #[test]
    fn fallback_module_404s_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fallback_module(dir.path(), &resolver(dir.path()), "/nope.ts").is_none());
    }

    fn resolver(root: &Path) -> OjResolver {
        OjResolver::with_conditions(root, &["import".to_string(), "default".to_string()])
    }

    #[test]
    fn resolve_fallback_serves_an_app_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/dep.ts"), "export const x = 1;\n").unwrap();
        let r = resolver(dir.path());
        match resolve_fallback(dir.path(), &r, &[], "/src/dep.ts", "./dep.ts", "/src/entry.tsx") {
            Fallback::Module { name, code } => {
                assert_eq!(name, "src/dep.ts");
                assert!(code.contains("export const x"), "{code}");
            }
            _ => panic!("expected a served module"),
        }
    }

    #[test]
    fn resolve_fallback_redirects_a_start_alias_to_its_target_file() {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("manifest-dev.ts"), "export const m = 1;\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/router.tsx"), "export const router = 1;\n").unwrap();
        let aliases = vec![
            ("tanstack-start-manifest:v".to_string(), assets.join("manifest-dev.ts")),
            ("#tanstack-router-entry".to_string(), dir.path().join("src/router")),
        ];
        let r = resolver(dir.path());
        match resolve_fallback(dir.path(), &r, &aliases, "/tanstack-start-manifest:v", "tanstack-start-manifest:v", "/e") {
            Fallback::Redirect { location } => assert!(location.ends_with("manifest-dev.ts"), "{location}"),
            _ => panic!("expected an alias redirect"),
        }
        // an extensionless alias target resolves through the extension probe
        match resolve_fallback(dir.path(), &r, &aliases, "/#tanstack-router-entry", "#tanstack-router-entry", "/e") {
            Fallback::Redirect { location } => assert!(location.ends_with("src/router.tsx"), "{location}"),
            _ => panic!("expected the router alias to resolve to router.tsx"),
        }
    }

    #[test]
    fn resolve_fallback_redirects_a_bare_import_to_its_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules/foo");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), r#"{"name":"foo","main":"index.js"}"#).unwrap();
        std::fs::write(pkg.join("index.js"), "export const foo = 1;\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let r = resolver(dir.path());
        match resolve_fallback(dir.path(), &r, &[], "/foo", "foo", "/src/entry.tsx") {
            Fallback::Redirect { location } => {
                assert!(location.ends_with("node_modules/foo/index.js"), "{location}");
                assert!(PathBuf::from(&location).is_absolute(), "{location}");
                // the redirect target then maps to a real file
                assert!(matches!(
                    resolve_fallback(dir.path(), &r, &[], &location, "foo", "/src/entry.tsx"),
                    Fallback::Module { .. }
                ));
            }
            _ => panic!("expected a redirect for the bare import"),
        }
    }

    #[test]
    fn fallback_wraps_a_cjs_node_modules_module_as_esm() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules/cjsdep");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("package.json"), r#"{"name":"cjsdep","main":"index.js"}"#).unwrap();
        std::fs::write(
            pkg.join("index.js"),
            "const os = require(\"os\");\nmodule.exports = function greet() { return \"cjs-ok\"; };\n",
        )
        .unwrap();
        let r = resolver(dir.path());
        let spec = pkg.join("index.js");
        let (_, code) =
            fallback_module(dir.path(), &r, &format!("/{}", spec.display())).unwrap();
        assert!(code.contains("export default"), "no ESM default export: {code}");
        assert!(code.contains("module.exports"), "cjs body not wrapped: {code}");
        // a node builtin require() is mapped to a node: import for nodejs_compat
        assert!(code.contains("\"node:os\""), "node builtin not mapped: {code}");
    }

    #[test]
    fn resolve_fallback_rewrites_a_hash_alias_import_to_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/router.tsx"), "export const router = 1;\n").unwrap();
        std::fs::write(
            dir.path().join("src/entry.tsx"),
            "import { router } from \"#tanstack-router-entry\";\nexport default router;\n",
        )
        .unwrap();
        let aliases =
            vec![("#tanstack-router-entry".to_string(), dir.path().join("src/router"))];
        let r = resolver(dir.path());
        match resolve_fallback(dir.path(), &r, &aliases, "/src/entry.tsx", "./entry", "/e") {
            Fallback::Module { code, .. } => {
                assert!(!code.contains("#tanstack-router-entry"), "hash alias survived: {code}");
                assert!(code.contains("src/router.tsx"), "not rewritten to target: {code}");
            }
            _ => panic!("expected the entry module served"),
        }
    }

    #[test]
    fn resolve_fallback_404s_an_unresolvable_bare_import() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path());
        assert!(matches!(
            resolve_fallback(dir.path(), &r, &[], "/ghost", "ghost", "/src/entry.tsx"),
            Fallback::NotFound
        ));
    }

    #[test]
    fn resolve_fallback_redirects_a_directory_index_to_its_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let comp = dir.path().join("src/tooltip");
        std::fs::create_dir_all(comp.join("patterns")).unwrap();
        std::fs::write(comp.join("index.tsx"), "export * from \"./patterns/x\";\n").unwrap();
        let r = resolver(dir.path());
        // a bare directory specifier must redirect to <dir>/index.tsx, not serve
        // inline under the extensionless name (which breaks relative children)
        let spec = format!("/{}", comp.display());
        match resolve_fallback(dir.path(), &r, &[], &spec, "./tooltip", "/src/entry.tsx") {
            Fallback::Redirect { location } => assert!(location.ends_with("tooltip/index.tsx"), "{location}"),
            _ => panic!("expected a directory-index redirect"),
        }
        // the redirected index path itself serves inline (no loop)
        let idx = format!("/{}", comp.join("index.tsx").display());
        assert!(matches!(
            resolve_fallback(dir.path(), &r, &[], &idx, "./tooltip", "/src/entry.tsx"),
            Fallback::Module { .. }
        ));
    }

    #[test]
    fn resolve_fallback_leaves_a_plugin_virtual_for_the_loader() {
        // a `virtual:` module is synthesized by a JS plugin, so the Rust tiers
        // must not claim it: it falls through to NotFound (then the async proxy).
        let dir = tempfile::tempdir().unwrap();
        let r = resolver(dir.path());
        assert!(matches!(
            resolve_fallback(dir.path(), &r, &[], "/virtual:greeting", "virtual:greeting", "/src/entry.tsx"),
            Fallback::NotFound
        ));
    }
}
