// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

pub const SIDECAR_JS: &str = include_str!("assets/tailwind-sidecar.mjs");
pub const PREPROCESS_JS: &str = include_str!("assets/css-preprocess.mjs");
pub const SVELTE_COMPILE_JS: &str = include_str!("assets/svelte-compile.mjs");

#[inline]
pub fn is_svelte(url: &str) -> bool {
    url.split('?').next().unwrap_or(url).ends_with(".svelte")
}

#[inline]
pub fn is_less(url: &str) -> bool {
    url.split('?').next().unwrap_or(url).ends_with(".less")
}

#[inline]
pub fn is_stylus(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    f.ends_with(".styl") || f.ends_with(".stylus")
}

pub fn is_tailwind_css(source: &str) -> bool {
    for (index, _) in source.match_indices('@') {
        let rest = &source[index..];
        if !at_directive_position(source, index) {
            continue;
        }
        if let Some(after) = rest.strip_prefix("@import") {
            let target = after.trim_start().trim_start_matches(['"', '\'']);
            if let Some(tail) = target.strip_prefix("tailwindcss") {
                if tail
                    .chars()
                    .next()
                    .is_none_or(|c| matches!(c, '"' | '\'' | '/') || c.is_whitespace())
                {
                    return true;
                }
            }
            continue;
        }
        for directive in ["@tailwind", "@theme", "@utility", "@apply", "@source"] {
            if let Some(after) = rest.strip_prefix(directive) {
                if after
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_')
                {
                    return true;
                }
            }
        }
    }
    false
}

fn at_directive_position(source: &str, index: usize) -> bool {
    source[..index]
        .chars()
        .rev()
        .find(|c| !matches!(c, ' ' | '\t'))
        .is_none_or(|c| matches!(c, '\n' | '\r' | '}' | '{' | ';'))
}

pub struct Sidecar {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    counter: AtomicU64,
    base: String,
    kind: &'static str,
    package: &'static str,
    _child: tokio::process::Child,
}

impl Sidecar {
    pub async fn spawn(root: &Path) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        Self::spawn_named(root, "tailwind-sidecar.mjs", SIDECAR_JS, "tailwind", "tailwindcss").await
    }

    pub async fn spawn_preprocess(root: &Path) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        Self::spawn_named(
            root,
            "css-preprocess.mjs",
            PREPROCESS_JS,
            "css preprocessor",
            "less or stylus",
        )
        .await
    }

    pub async fn spawn_svelte(root: &Path) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        Self::spawn_named(
            root,
            "svelte-compile.mjs",
            SVELTE_COMPILE_JS,
            "svelte compiler",
            "svelte",
        )
        .await
    }

    async fn spawn_named(
        root: &Path,
        name: &str,
        js: &str,
        kind: &'static str,
        package: &'static str,
    ) -> anyhow::Result<std::sync::Arc<Sidecar>> {
        let script = oj_cache::cache_root(&root).join(name);
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, js)?;

        let mut child = tokio::process::Command::new("node")
            .arg(&script)
            .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
            .env("NODE_COMPILE_CACHE", crate::node_compile_cache(root))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot spawn node for the {kind} sidecar: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let sidecar = std::sync::Arc::new(Sidecar {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            base: root.display().to_string(),
            kind,
            package,
            _child: child,
        });

        let reader_ref = std::sync::Arc::clone(&sidecar);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                let Some(id) = msg["id"].as_u64() else {
                    continue;
                };
                let result = match msg["css"].as_str() {
                    Some(css) => Ok(css.to_string()),
                    None => Err(msg["error"].as_str().unwrap_or("sidecar error").to_string()),
                };
                if let Some(tx) = reader_ref.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(result);
                }
            }
        });
        Ok(sidecar)
    }

    pub async fn compile(&self, css: &str, from: &str) -> Result<String, String> {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let request = serde_json::json!({ "id": id, "base": self.base, "css": css, "from": from });
        {
            let mut stdin = self.stdin.lock().await;
            if stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .is_err()
            {
                self.pending.lock().unwrap().remove(&id);
                return Err(format!(
                    "{} sidecar died (is {} installed?)",
                    self.kind, self.package
                ));
            }
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            other => {
                self.pending.lock().unwrap().remove(&id);
                Err(match other {
                    Ok(Err(_)) => format!("{} sidecar closed the connection", self.kind),
                    _ => format!(
                        "{} sidecar timed out after {}s",
                        self.kind,
                        REQUEST_TIMEOUT.as_secs()
                    ),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailwind_imports_are_detected_in_every_written_form() {
        for source in [
            "@import \"tailwindcss\";",
            "@import 'tailwindcss';",
            "@import \"tailwindcss\"",
            "  @import \"tailwindcss\";",
            "\n\n@import \"tailwindcss\";\n",
            // v4 subpath and layer forms.
            "@import \"tailwindcss/preflight\" layer(base);",
            "@import \"tailwindcss/theme\" layer(theme);",
            "@import \"tailwindcss/utilities\" layer(utilities);",
            // Other statements around it.
            "@charset \"utf-8\";\n@import \"tailwindcss\";",
            ".a { color: red }\n@import \"tailwindcss\";",
        ] {
            assert!(is_tailwind_css(source), "missed: {source:?}");
        }
    }

    #[test]
    fn tailwind_directives_are_detected_only_where_a_directive_can_start() {
        for source in [
            "@tailwind base;",
            "@tailwind utilities;",
            "@theme { --color-brand: red }",
            "@theme inline { --x: 1 }",
            ".btn {\n  @apply underline;\n}",
            "@utility tab-4 { tab-size: 4 }",
            "@source \"./src/**/*.tsx\";",
            "}\n@theme { --x: 1 }",
        ] {
            assert!(is_tailwind_css(source), "missed: {source:?}");
        }
    }

    #[test]
    fn a_plain_stylesheet_never_reaches_the_tailwind_sidecar() {
        // A false positive here fails the build with "is tailwindcss installed?"
        // on a stylesheet that has nothing to do with Tailwind.
        for source in [
            "",
            ".a { color: red }",
            "@media (min-width: 1px) { .a { color: red } }",
            "@supports (display: grid) { .a { display: grid } }",
            "@font-face { font-family: X; src: url(x.woff2) }",
            "@keyframes spin { to { transform: rotate(1turn) } }",
            "@layer base { .a { color: red } }",
            "@import \"./other.css\";",
            "@import \"@acme/design/tokens.css\";",
            // The word appears, but not as a directive.
            "/* @theme is not used here */",
            "/* @tailwind base; */",
            ".a { content: \"@theme\" }",
            ".a { content: \"@import \\\"tailwindcss\\\"\" }",
            "@themes { --x: 1 }",
            "@theming { --x: 1 }",
            ".a[data-x=\"@apply\"] { color: red }",
            "/* see @source for details */",
            "@import \"tailwindcss-is-not-this-package\";",
        ] {
            assert!(!is_tailwind_css(source), "false positive: {source:?}");
        }
    }

    #[test]
    fn an_import_of_a_package_named_after_tailwind_is_not_tailwind() {
        // The prefix has to end at a boundary the package itself uses.
        assert!(is_tailwind_css("@import \"tailwindcss\";"));
        assert!(is_tailwind_css("@import \"tailwindcss/theme\";"));
        assert!(!is_tailwind_css("@import \"tailwindcss-animate\";"));
        assert!(!is_tailwind_css("@import \"my-tailwindcss\";"));
    }

    #[test]
    fn style_dialects_are_classified_by_extension_not_by_query() {
        assert!(is_less("/src/a.less"));
        assert!(is_less("/src/a.less?inline"));
        assert!(!is_less("/src/a.less/b.css"));
        assert!(!is_less("/src/a.css?x=.less"));

        assert!(is_stylus("/src/a.styl"));
        assert!(is_stylus("/src/a.stylus"));
        assert!(is_stylus("/src/a.styl?raw"));
        assert!(!is_stylus("/src/a.style"));
        assert!(!is_stylus("/src/a.css?x=.styl"));

        assert!(is_svelte("/src/A.svelte"));
        assert!(is_svelte("/src/A.svelte?raw"));
        assert!(!is_svelte("/src/A.svelte.ts"));
        assert!(!is_svelte("/src/a.ts?x=.svelte"));

        for predicate in [is_less, is_stylus, is_svelte] {
            assert!(!predicate(""));
            assert!(!predicate("?"));
            assert!(!predicate("/"));
        }
    }
}
