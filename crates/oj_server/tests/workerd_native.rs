// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! End-to-end: oj's workerd orchestration boots real workerd and answers its
//! module-fallback requests with TS/JSX-stripped ESM, so a route renders inside
//! the Cloudflare runtime — no Node, Miniflare, or Vite plugin. Skips when the
//! `workerd` binary is not present.

use std::time::Duration;

use oj_server::workerd::find_workerd;
use oj_server::workerd_dev::{spawn, WorkerdSpawn};

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
    let pkg = root.join("node_modules/banner");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("package.json"), "{\"name\":\"banner\",\"main\":\"index.js\"}").unwrap();
    std::fs::write(pkg.join("index.js"), "export const suffix = \" +node_modules\";\n").unwrap();
    std::fs::write(
        root.join("src/entry.tsx"),
        "import { greeting } from \"./dep.ts\";\n\
         import { suffix } from \"banner\";\n\
         interface Env {}\n\
         export default {\n\
           fetch(_req: Request, _env: Env): Response {\n\
             return new Response(greeting + \" [ts-stripped]\" + suffix);\n\
           },\n\
         };\n",
    )
    .unwrap();

    let session = spawn(
        &root,
        &workerd,
        &root.join(".oj-cache"),
        vec![],
        WorkerdSpawn {
            compat_date: "2024-11-01".into(),
            compat_flags: vec![],
            entry_specifier: "/src/entry.tsx".into(),
            vars: vec![],
            service_bindings: vec![],
        },
    )
    .await
    .expect("spawn workerd session");

    let mut body = String::new();
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if let Ok(resp) = client.get(session.worker_url()).send().await {
            if resp.status().is_success() {
                body = resp.text().await.unwrap_or_default();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        body.contains("rendered by real workerd [ts-stripped] +node_modules"),
        "workerd did not render the TS route (with its node_modules import) through the orchestration.\n--- body ---\n{body}",
    );
}

#[tokio::test]
async fn native_workerd_resolves_a_colon_scheme_alias() {
    let app = tempfile::tempdir().unwrap();
    let root = app.path().to_path_buf();

    let Some(workerd) = find_workerd(&root) else {
        eprintln!("SKIP workerd_native alias: workerd binary not found (set OJ_WORKERD_BIN)");
        return;
    };

    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(
        assets.join("manifest-dev.ts"),
        "export const manifest: string = \" +manifest-alias\";\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/entry.tsx"),
        "import { manifest } from \"tanstack-start-manifest:v\";\n\
         export default {\n\
           fetch(): Response {\n\
             return new Response(\"aliased\" + manifest);\n\
           },\n\
         };\n",
    )
    .unwrap();

    let aliases =
        vec![("tanstack-start-manifest:v".to_string(), assets.join("manifest-dev.ts"))];

    let session = spawn(
        &root,
        &workerd,
        &root.join(".oj-cache"),
        aliases,
        WorkerdSpawn {
            compat_date: "2024-11-01".into(),
            compat_flags: vec![],
            entry_specifier: "/src/entry.tsx".into(),
            vars: vec![],
            service_bindings: vec![],
        },
    )
    .await
    .expect("spawn workerd session");

    let mut body = String::new();
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if let Ok(resp) = client.get(session.worker_url()).send().await {
            if resp.status().is_success() {
                body = resp.text().await.unwrap_or_default();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        body.contains("aliased +manifest-alias"),
        "workerd did not resolve the colon-scheme alias through the fallback.\n--- body ---\n{body}",
    );
}

#[tokio::test]
async fn native_workerd_rewrites_a_hash_alias_import() {
    let app = tempfile::tempdir().unwrap();
    let root = app.path().to_path_buf();

    let Some(workerd) = find_workerd(&root) else {
        eprintln!("SKIP workerd_native hash-alias: workerd binary not found (set OJ_WORKERD_BIN)");
        return;
    };

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/router.tsx"),
        "export const router: string = \" +from-router\";\n",
    )
    .unwrap();
    // workerd never sends a "#" subpath import to the module fallback, so this
    // route can only render if the fallback rewrote the specifier to a path.
    std::fs::write(
        root.join("src/entry.tsx"),
        "import { router } from \"#tanstack-router-entry\";\n\
         export default {\n\
           fetch(): Response {\n\
             return new Response(\"hash-alias\" + router);\n\
           },\n\
         };\n",
    )
    .unwrap();

    let aliases =
        vec![("#tanstack-router-entry".to_string(), root.join("src/router"))];

    let session = spawn(
        &root,
        &workerd,
        &root.join(".oj-cache"),
        aliases,
        WorkerdSpawn {
            compat_date: "2024-11-01".into(),
            compat_flags: vec![],
            entry_specifier: "/src/entry.tsx".into(),
            vars: vec![],
            service_bindings: vec![],
        },
    )
    .await
    .expect("spawn workerd session");

    let mut body = String::new();
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if let Ok(resp) = client.get(session.worker_url()).send().await {
            if resp.status().is_success() {
                body = resp.text().await.unwrap_or_default();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        body.contains("hash-alias +from-router"),
        "workerd did not render a route whose entry uses a # subpath alias (rewrite failed).\n--- body ---\n{body}",
    );
}

#[tokio::test]
async fn native_workerd_imports_a_cjs_node_modules_package() {
    let app = tempfile::tempdir().unwrap();
    let root = app.path().to_path_buf();

    let Some(workerd) = find_workerd(&root) else {
        eprintln!("SKIP workerd_native cjs: workerd binary not found (set OJ_WORKERD_BIN)");
        return;
    };

    // a CommonJS package: no import/export, module.exports assigned. workerd's
    // ESM loader cannot evaluate this directly; the fallback must wrap it.
    let leaf = root.join("node_modules/cjsleaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(leaf.join("package.json"), "{\"name\":\"cjsleaf\",\"main\":\"index.js\"}").unwrap();
    std::fs::write(leaf.join("index.js"), "module.exports = \" +leaf\";\n").unwrap();

    let pkg = root.join("node_modules/cjspkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(pkg.join("package.json"), "{\"name\":\"cjspkg\",\"main\":\"index.js\"}").unwrap();
    // requires another CJS package, exercising require()-to-import interop
    std::fs::write(
        pkg.join("index.js"),
        "const leaf = require(\"cjsleaf\");\nmodule.exports = { tag: \"cjs-rendered\" + leaf };\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/entry.tsx"),
        "import pkg from \"cjspkg\";\n\
         export default {\n\
           fetch(): Response {\n\
             return new Response(pkg.tag);\n\
           },\n\
         };\n",
    )
    .unwrap();

    let session = spawn(
        &root,
        &workerd,
        &root.join(".oj-cache"),
        vec![],
        WorkerdSpawn {
            compat_date: "2024-11-01".into(),
            compat_flags: vec![],
            entry_specifier: "/src/entry.tsx".into(),
            vars: vec![],
            service_bindings: vec![],
        },
    )
    .await
    .expect("spawn workerd session");

    let mut body = String::new();
    let client = reqwest::Client::new();
    for _ in 0..60 {
        if let Ok(resp) = client.get(session.worker_url()).send().await {
            if resp.status().is_success() {
                body = resp.text().await.unwrap_or_default();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        body.contains("cjs-rendered +leaf"),
        "workerd did not render a CJS node_modules package (transitive require) through the fallback.\n--- body ---\n{body}",
    );
}
