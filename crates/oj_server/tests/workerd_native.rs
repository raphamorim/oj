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
