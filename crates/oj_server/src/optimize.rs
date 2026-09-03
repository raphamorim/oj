// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::watch;

const OPTIMIZE_JS: &str = include_str!("assets/optimize-deps.mjs");

pub struct DepMeta {
    pub file: String,
    pub needs_interop: bool,
}

pub type DepMap = HashMap<String, DepMeta>;

pub struct OptimizedDeps {
    rx: watch::Receiver<Option<Arc<DepMap>>>,
    dir: PathBuf,
}

impl OptimizedDeps {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn ready(&self) -> Arc<DepMap> {
        let mut rx = self.rx.clone();
        loop {
            if let Some(map) = rx.borrow().clone() {
                return map;
            }
            if rx.changed().await.is_err() {
                return Arc::new(DepMap::new());
            }
        }
    }

    pub fn disabled() -> Self {
        let (_tx, rx) = watch::channel(Some(Arc::new(DepMap::new())));
        OptimizedDeps {
            rx,
            dir: PathBuf::new(),
        }
    }

    pub fn prepare(root: &Path, version: &str, input: OptimizeInput) -> Self {
        let dir = oj_cache::cache_root(&root).join("deps");
        let hash = lockfile_hash(root, version, &input);
        let (tx, rx) = watch::channel(None);

        // optimizeDeps.force: ignore any cached pre-bundle and always rebuild.
        if !input.force {
            if let Some(map) = load_manifest(&dir, &hash) {
                let _ = tx.send(Some(Arc::new(map)));
                return OptimizedDeps { rx, dir };
            }
        }

        let root = root.to_path_buf();
        let dir_task = dir.clone();
        tokio::spawn(async move {
            let map = run_optimizer(&root, &dir_task, &hash, &input)
                .await
                .unwrap_or_default();
            let _ = tx.send(Some(Arc::new(map)));
        });
        OptimizedDeps { rx, dir }
    }
}

/// Dependency-optimizer inputs derived from the resolved config
/// (`optimizeDeps.include/exclude/entries`, `resolve.dedupe`, `resolve.alias`).
#[derive(Default, Clone)]
pub struct OptimizeInput {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub entries: Vec<String>,
    pub dedupe: Vec<String>,
    pub alias: Vec<(String, String)>,
    /// `optimizeDeps.force`: bypass the cached pre-bundle and rebuild.
    pub force: bool,
    /// `optimizeDeps.esbuildOptions`/`rolldownOptions`: forwarded to the sidecar.
    pub bundler_options: Option<serde_json::Value>,
    /// The Rust resolver's settings, so the pre-bundle resolves every dep to the
    /// same file the dev server serves (Vite uses one resolver for both).
    pub conditions: Vec<String>,
    pub main_fields: Vec<String>,
    pub extensions: Vec<String>,
    pub preserve_symlinks: bool,
    /// Vite's `--mode` (getConfigHash folds `define: NODE_ENV || mode`): a dep
    /// prebundled for `development` is not the `production` one.
    pub mode: String,
}

/// Vite's lockfileFormats (optimizer/index.ts): the lockfile a package manager
/// writes, paired with the patch-package directory whose mtime must also
/// invalidate the prebundle (`checkPatchesDir`).
const LOCKFILES: &[(&str, Option<&str>)] = &[
    ("node_modules/.pnpm/lock.yaml", None),
    ("node_modules/.package-lock.json", Some("patches")),
    ("node_modules/.yarn-state.yml", None),
    ("bun.lock", Some("patches")),
    (".rush/temp/shrinkwrap-deps.json", None),
    ("aube-lock.yaml", None),
    ("nub.lock", Some("patches")),
    (".pnp.cjs", Some(".yarn/patches")),
    (".pnp.js", Some(".yarn/patches")),
    ("node_modules/.yarn-integrity", Some("patches")),
    ("bun.lockb", Some("patches")),
    // Top-level lockfiles oj has always keyed on (Vite reads the installed
    // node_modules mirrors above instead; hashing both is a superset).
    ("package-lock.json", Some("patches")),
    ("yarn.lock", Some(".yarn/patches")),
    ("pnpm-lock.yaml", None),
    ("deno.lock", None),
];

/// Fold every lockfile found in the nearest ancestor directory that has one
/// (Vite's lookupFile walks up from root), plus the mtime of its patch-package
/// directory, into `hasher`.
fn hash_lockfiles(root: &Path, hasher: &mut blake3::Hasher) {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let mut found = false;
        for (name, patches) in LOCKFILES {
            if let Ok(bytes) = std::fs::read(d.join(name)) {
                found = true;
                hasher.update(name.as_bytes());
                hasher.update(&bytes);
                if let Some(patches) = patches {
                    if let Ok(meta) = std::fs::metadata(d.join(patches)) {
                        if meta.is_dir() {
                            if let Ok(mtime) = meta.modified() {
                                hasher.update(b"\0p");
                                hasher.update(format!("{mtime:?}").as_bytes());
                            }
                        }
                    }
                }
            }
        }
        if found {
            return;
        }
        dir = d.parent();
    }
}

fn lockfile_hash(root: &Path, version: &str, input: &OptimizeInput) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(version.as_bytes());
    hash_lockfiles(root, &mut hasher);
    if let Ok(bytes) = std::fs::read(root.join("package.json")) {
        hasher.update(b"package.json");
        hasher.update(&bytes);
    }
    hasher.update(b"\0mode=");
    hasher.update(input.mode.as_bytes());
    // Fold the optimizer config into the key so include/exclude/entries/dedupe/alias
    // changes invalidate a stale prebundle.
    for (tag, list) in [
        (b"\0i".as_slice(), &input.include),
        (b"\0x".as_slice(), &input.exclude),
        (b"\0e".as_slice(), &input.entries),
        (b"\0d".as_slice(), &input.dedupe),
    ] {
        for entry in list {
            hasher.update(tag);
            hasher.update(entry.as_bytes());
        }
    }
    for (find, replacement) in &input.alias {
        hasher.update(b"\0a");
        hasher.update(find.as_bytes());
        hasher.update(b"=");
        hasher.update(replacement.as_bytes());
    }
    if let Some(opts) = &input.bundler_options {
        hasher.update(b"\0o");
        hasher.update(opts.to_string().as_bytes());
    }
    for (tag, list) in [
        (b"\0c".as_slice(), &input.conditions),
        (b"\0m".as_slice(), &input.main_fields),
        (b"\0t".as_slice(), &input.extensions),
    ] {
        for entry in list {
            hasher.update(tag);
            hasher.update(entry.as_bytes());
        }
    }
    hasher.update(&[b'\0', b's', input.preserve_symlinks as u8]);
    hasher.finalize().to_hex().to_string()
}

fn parse_metadata(v: &serde_json::Value) -> Option<DepMap> {
    let obj = v.as_object()?;
    let mut map = DepMap::new();
    for (dep, meta) in obj {
        let file = meta.get("file")?.as_str()?.to_string();
        let needs_interop = meta
            .get("needsInterop")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        map.insert(
            dep.clone(),
            DepMeta {
                file,
                needs_interop,
            },
        );
    }
    Some(map)
}

fn load_manifest(dir: &Path, hash: &str) -> Option<DepMap> {
    let raw = std::fs::read_to_string(dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("hash")?.as_str()? != hash {
        return None;
    }
    let map = parse_metadata(v.get("metadata")?)?;
    for m in map.values() {
        if !dir.join(&m.file).exists() {
            return None;
        }
    }
    Some(map)
}

async fn run_optimizer(
    root: &Path,
    dir: &Path,
    hash: &str,
    input: &OptimizeInput,
) -> Option<DepMap> {
    let cache = oj_cache::cache_root(&root);
    std::fs::create_dir_all(&cache).ok()?;
    let script = cache.join("optimize-deps.mjs");
    std::fs::write(&script, OPTIMIZE_JS).ok()?;
    // react/jsx-dev-runtime is always prebundled (oj injects the dev JSX runtime);
    // merge it with any user optimizeDeps.include.
    let mut include = vec!["react/jsx-dev-runtime".to_string()];
    for dep in &input.include {
        if !include.contains(dep) {
            include.push(dep.clone());
        }
    }
    let alias: Vec<[&str; 2]> = input
        .alias
        .iter()
        .map(|(f, r)| [f.as_str(), r.as_str()])
        .collect();
    // Full-graph auto-discovery (esbuild-scan the whole dep tree and pre-bundle
    // it) is opt-in via OJ_OPTIMIZE_SCAN=1: it can break apps with UMD/CommonJS
    // interop quirks, so by default oj pre-bundles only the explicit
    // optimizeDeps.include list and serves the rest through wrap_cjs.
    let auto_discover = std::env::var("OJ_OPTIMIZE_SCAN")
        .is_ok_and(|v| !v.is_empty() && v != "0");
    let cfg = serde_json::json!({
        "root": root,
        "outDir": dir,
        "entries": input.entries,
        "include": include,
        "exclude": input.exclude,
        "dedupe": input.dedupe,
        "alias": alias,
        "autoDiscover": auto_discover,
        "esbuildOptions": input.bundler_options,
        "resolve": {
            "conditions": input.conditions,
            "mainFields": input.main_fields,
            "extensions": input.extensions,
            "preserveSymlinks": input.preserve_symlinks,
        },
    })
    .to_string();
    let out = tokio::process::Command::new("node")
        .arg(&script)
        .arg(&cfg)
        .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "oj: optimizer exited {:?}\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "oj: optimizer stdout not JSON: {e}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
    };
    let metadata = v.get("metadata")?;
    let map = parse_metadata(metadata)?;
    let manifest = serde_json::json!({ "hash": hash, "metadata": metadata });
    let _ = std::fs::write(dir.join("manifest.json"), manifest.to_string());
    Some(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(include: &[&str], exclude: &[&str], entries: &[&str], dedupe: &[&str]) -> OptimizeInput {
        OptimizeInput {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            entries: entries.iter().map(|s| s.to_string()).collect(),
            dedupe: dedupe.iter().map(|s| s.to_string()).collect(),
            alias: Vec::new(),
            ..Default::default()
        }
    }

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    #[test]
    fn every_optimizer_list_is_distinguishable_in_the_key() {
        let dir = project(&[("package.json", r#"{"name":"app"}"#)]);
        let root = dir.path();
        let key = |i: &OptimizeInput| lockfile_hash(root, "0.0.1", i);

        // The same package in a different list must not reuse a prebundle: an
        // excluded dep is not a prebundled one.
        let base = key(&input(&["react"], &[], &[], &[]));
        assert_ne!(base, key(&input(&[], &["react"], &[], &[])), "include vs exclude");
        assert_ne!(base, key(&input(&[], &[], &["react"], &[])), "include vs entries");
        assert_ne!(base, key(&input(&[], &[], &[], &["react"])), "include vs dedupe");
        // ...and neither may a list boundary that merely moves an item across it.
        assert_ne!(
            key(&input(&["a"], &["b"], &[], &[])),
            key(&input(&["a", "b"], &[], &[], &[])),
            "moving an entry across the include/exclude boundary"
        );
        assert_ne!(
            key(&input(&[], &[], &["a", "b"], &[])),
            key(&input(&[], &[], &["a"], &["b"])),
            "moving an entry across the entries/dedupe boundary"
        );
    }

    #[test]
    fn the_key_covers_the_lockfiles_the_version_and_the_aliases() {
        let dir = project(&[
            ("package.json", r#"{"name":"app","dependencies":{"react":"18"}}"#),
            ("package-lock.json", r#"{"lockfileVersion":3}"#),
        ]);
        let root = dir.path();
        let empty = OptimizeInput::default();
        let base = lockfile_hash(root, "0.0.1", &empty);

        assert_eq!(base, lockfile_hash(root, "0.0.1", &empty), "deterministic");
        assert_ne!(base, lockfile_hash(root, "0.0.2", &empty), "tool version");

        std::fs::write(root.join("package-lock.json"), r#"{"lockfileVersion":4}"#).unwrap();
        let after_lock = lockfile_hash(root, "0.0.1", &empty);
        assert_ne!(base, after_lock, "a lockfile change must invalidate");

        let dev = OptimizeInput {
            mode: "development".into(),
            ..OptimizeInput::default()
        };
        let prod = OptimizeInput {
            mode: "production".into(),
            ..OptimizeInput::default()
        };
        assert_ne!(lockfile_hash(root, "0.0.1", &dev), lockfile_hash(root, "0.0.1", &prod), "mode");

        let aliased = OptimizeInput {
            alias: vec![("~".into(), "./src".into())],
            ..OptimizeInput::default()
        };
        assert_ne!(after_lock, lockfile_hash(root, "0.0.1", &aliased), "alias");
        let swapped = OptimizeInput {
            alias: vec![("./src".into(), "~".into())],
            ..OptimizeInput::default()
        };
        assert_ne!(
            lockfile_hash(root, "0.0.1", &aliased),
            lockfile_hash(root, "0.0.1", &swapped),
            "an alias is directional"
        );
    }

    #[test]
    fn every_vite_lockfile_format_and_the_patches_dir_key_the_prebundle() {
        let dir = project(&[("package.json", r#"{"name":"app"}"#)]);
        let root = dir.path();
        let empty = OptimizeInput::default();
        let key = || lockfile_hash(root, "v", &empty);
        let bare = key();

        // Text bun.lock and deno.lock (audit: only bun.lockb was keyed).
        std::fs::write(root.join("bun.lock"), "{}").unwrap();
        let bun = key();
        assert_ne!(bare, bun, "bun.lock");
        std::fs::write(root.join("bun.lock"), "{\"a\":1}").unwrap();
        assert_ne!(bun, key(), "bun.lock content");
        std::fs::remove_file(root.join("bun.lock")).unwrap();
        std::fs::write(root.join("deno.lock"), "{}").unwrap();
        assert_ne!(bare, key(), "deno.lock");
        std::fs::remove_file(root.join("deno.lock")).unwrap();

        // The installed mirror Vite reads (node_modules/.package-lock.json).
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/.package-lock.json"), "{}").unwrap();
        let installed = key();
        assert_ne!(bare, installed, "node_modules/.package-lock.json");

        // patch-package: a `patches/` directory next to an npm lockfile is part
        // of the key (Vite's checkPatchesDir), so re-patching a dep re-bundles.
        std::fs::create_dir_all(root.join("patches")).unwrap();
        let patched = key();
        assert_ne!(installed, patched, "patches dir");
        assert_eq!(patched, key(), "deterministic while nothing changes");
    }

    #[test]
    fn the_lockfile_is_looked_up_in_ancestor_directories() {
        // A workspace package's prebundle keys on the monorepo lockfile above it
        // (Vite's lookupFile walks up from root).
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("packages/app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("package.json"), r#"{"name":"app"}"#).unwrap();
        let empty = OptimizeInput::default();
        let before = lockfile_hash(&app, "v", &empty);
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        let after = lockfile_hash(&app, "v", &empty);
        assert_ne!(before, after, "ancestor lockfile is keyed");
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "lockfileVersion: 10").unwrap();
        assert_ne!(after, lockfile_hash(&app, "v", &empty), "and its content matters");
    }

    #[test]
    fn a_manifest_is_only_reused_when_its_hash_and_its_files_are_there() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(deps.join("react.js"), "export default 1;").unwrap();
        std::fs::write(
            deps.join("manifest.json"),
            r#"{"hash":"abc","metadata":{"react":{"file":"react.js","needsInterop":false}}}"#,
        )
        .unwrap();

        let map = load_manifest(&deps, "abc").expect("matching hash loads");
        assert_eq!(map["react"].file, "react.js");
        assert!(!map["react"].needs_interop);

        assert!(load_manifest(&deps, "different").is_none(), "stale hash");

        // A manifest whose prebundle is gone is not a warm cache.
        std::fs::remove_file(deps.join("react.js")).unwrap();
        assert!(load_manifest(&deps, "abc").is_none(), "missing dep file");
    }

    #[test]
    fn a_malformed_manifest_is_a_miss_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        for contents in [
            "",
            "{",
            "null",
            "[]",
            r#"{"metadata":{}}"#,
            r#"{"hash":"abc"}"#,
            r#"{"hash":123,"metadata":{}}"#,
            r#"{"hash":"abc","metadata":[]}"#,
            r#"{"hash":"abc","metadata":{"react":{}}}"#,
            r#"{"hash":"abc","metadata":{"react":{"file":42}}}"#,
        ] {
            std::fs::write(deps.join("manifest.json"), contents).unwrap();
            assert!(
                load_manifest(&deps, "abc").is_none(),
                "accepted {contents:?}"
            );
        }
        // An empty metadata object is a legitimate warm cache with no deps.
        std::fs::write(
            deps.join("manifest.json"),
            r#"{"hash":"abc","metadata":{}}"#,
        )
        .unwrap();
        assert!(load_manifest(&deps, "abc").expect("empty is valid").is_empty());
    }

    #[test]
    fn needs_interop_defaults_to_true_when_the_optimizer_does_not_say() {
        // Interop is the safe default: assuming an ESM dep needs none would
        // break `import x from "cjs-dep"` at runtime.
        let v: serde_json::Value =
            serde_json::from_str(r#"{"dep":{"file":"dep.js"}}"#).unwrap();
        let map = parse_metadata(&v).unwrap();
        assert!(map["dep"].needs_interop);
    }

    #[tokio::test]
    async fn a_disabled_optimizer_is_ready_immediately_and_empty() {
        let deps = OptimizedDeps::disabled();
        assert!(deps.ready().await.is_empty());
        assert_eq!(deps.dir(), Path::new(""));
    }

    #[test]
    fn resolve_settings_change_the_prebundle_hash() {
        let dir = std::env::temp_dir().join(format!("oj-opt-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = OptimizeInput {
            include: vec!["dep".into()],
            conditions: vec!["browser".into(), "import".into()],
            ..Default::default()
        };
        let mut with_dev = base.clone();
        with_dev.conditions.push("development".into());
        let mut fields = base.clone();
        fields.main_fields = vec!["main".into()];
        let mut links = base.clone();
        links.preserve_symlinks = true;
        let h = |i: &OptimizeInput| lockfile_hash(&dir, "v", i);
        assert_ne!(h(&base), h(&with_dev));
        assert_ne!(h(&base), h(&fields));
        assert_ne!(h(&base), h(&links));
        assert_eq!(h(&base), h(&base.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
