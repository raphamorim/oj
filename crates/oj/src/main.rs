// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

mod build;
mod ssr_dev;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "oj", version, about = "A Rust-native build tool for React apps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the dev server
    Dev {
        /// App root containing index.html (defaults to ./playground when present)
        root: Option<PathBuf>,
        /// Dev server port (overrides oj.config server.port; default 5199)
        #[arg(long)]
        port: Option<u16>,
        /// Full bundle mode: serve one registry-runtime chunk instead of
        /// native ESM modules (experimental)
        #[arg(long)]
        bundle: bool,
        /// SSR dev mode: render this entry (exporting `render(): string`)
        /// server-side, rebuilding on change with full page reload
        #[arg(long)]
        ssr: Option<String>,
    },
    /// Compile one file and print the output (debugging aid)
    Compile {
        file: PathBuf,
        /// Production transform (no jsxDEV, no Fast Refresh instrumentation)
        #[arg(long)]
        prod: bool,
    },
    /// Production build (embedded Rolldown: shake, chunk, minify, hash)
    Build {
        /// App root containing index.html (defaults to ./playground when present)
        root: Option<PathBuf>,
        /// Output directory (overrides oj.config build.outDir; default dist)
        #[arg(long)]
        out: Option<PathBuf>,
        /// SSR entry: build a Node server bundle (overrides oj.config build.ssr)
        #[arg(long)]
        ssr: Option<String>,
    },
    /// Preview a production build (static server over the build dir)
    Preview {
        /// App root (defaults to ./playground when present)
        root: Option<PathBuf>,
        /// Build dir to serve (overrides oj.config build.outDir; default dist)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Preview port (overrides oj.config preview.port; default 4173)
        #[arg(long)]
        port: Option<u16>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Dev { root, port, bundle, ssr } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() { playground } else { PathBuf::from(".") }
            });
            if let Some(entry) = ssr {
                ssr_dev::ssr_dev(root, entry, port).await
            } else {
                oj_server::DevServer { root, port, bundle }.run().await
            }
        }
        Command::Compile { file, prod } => {
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("cannot read {}", file.display()))?;
            let opts = if prod {
                oj_compiler::CompileOptions::prod()
            } else {
                oj_compiler::CompileOptions::dev()
            };
            let output = oj_compiler::compile(&file, &source, &opts)?;
            println!("{}", output.code);
            Ok(())
        }
        Command::Build { root, out, ssr } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() { playground } else { PathBuf::from(".") }
            });
            build::build(root, out, ssr).await
        }
        Command::Preview { root, out, port } => {
            let root = root
                .unwrap_or_else(|| {
                    let playground = PathBuf::from("playground");
                    if playground.join("index.html").is_file() {
                        playground
                    } else {
                        PathBuf::from(".")
                    }
                })
                .canonicalize()
                .with_context(|| "app root not found")?;
            let config = oj_config::load(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
            let out_dir = out
                .or_else(|| config.build.as_ref().and_then(|b| b.out_dir.as_ref()).map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("dist"));
            let out_dir = if out_dir.is_absolute() { out_dir } else { root.join(out_dir) };
            let port = port
                .or_else(|| config.preview.as_ref().and_then(|p| p.port))
                .unwrap_or(4173);
            let base = config.base.clone().unwrap_or_else(|| "/".into());
            oj_server::preview(out_dir, port, base).await
        }
    }
}
