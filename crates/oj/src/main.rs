// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

mod build;
mod ssr_dev;
mod start_dev;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "oj",
    version,
    about = "A Rust-native build tool for React apps"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Dev {
        root: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        bundle: bool,
        #[arg(long)]
        ssr: Option<String>,
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
        host: Option<String>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Enable the experimental on-disk module cache (also OJ_ENABLE_CACHE=1).
        /// Off by default; warm restarts then re-serve compiled modules from disk.
        #[arg(long)]
        enable_cache: bool,
        /// Force the on-disk module cache off even if enabled (also OJ_NO_CACHE=1).
        #[arg(long)]
        no_cache: bool,
        /// Compile modules on demand instead of eagerly crawling the whole graph
        /// on boot. Opt-in: the eager crawl pre-compiles in parallel and warms
        /// HMR, which is usually the faster default; --lazy suits apps that
        /// code-split so the first route needs only a fraction of the graph.
        #[arg(long)]
        lazy: bool,
    },
    Compile {
        file: PathBuf,
        #[arg(long)]
        prod: bool,
    },
    Build {
        root: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        ssr: Option<String>,
        #[arg(long)]
        mode: Option<String>,
    },
    Preview {
        root: Option<PathBuf>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
        host: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(oj_compiler::COMPILE_STACK_SIZE)
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Dev {
            root,
            port,
            bundle,
            ssr,
            host,
            config,
            enable_cache,
            no_cache,
            lazy,
        } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() {
                    playground
                } else {
                    PathBuf::from(".")
                }
            });
            if let Some(entry) = ssr {
                ssr_dev::ssr_dev(root, entry, port, host).await
            } else if oj_server::is_tanstack_start_app(&root) {
                start_dev::start_dev(root, port, host).await
            } else {
                oj_server::DevServer {
                    root,
                    port,
                    bundle,
                    host,
                    config,
                    enable_cache,
                    no_cache,
                    lazy,
                }
                .run()
                .await
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
        Command::Build {
            root,
            out,
            ssr,
            mode,
        } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() {
                    playground
                } else {
                    PathBuf::from(".")
                }
            });
            let mode = mode.unwrap_or_else(|| "production".to_string());
            if oj_server::is_tanstack_start_app(&root) {
                start_dev::start_build(root, &mode).await
            } else {
                build::build(root, out, ssr, &mode).await
            }
        }
        Command::Preview {
            root,
            out,
            port,
            host,
        } => {
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
                .or_else(|| {
                    config
                        .build
                        .as_ref()
                        .and_then(|b| b.out_dir.as_ref())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| PathBuf::from("dist"));
            let out_dir = if out_dir.is_absolute() {
                out_dir
            } else {
                root.join(out_dir)
            };
            let port = port
                .or_else(|| config.preview.as_ref().and_then(|p| p.port))
                .unwrap_or(4173);
            let base = config.base.clone().unwrap_or_else(|| "/".into());
            let headers: Vec<(String, String)> = config
                .preview
                .as_ref()
                .and_then(|p| p.headers.clone())
                .or_else(|| config.server.as_ref().and_then(|s| s.headers.clone()))
                .map(|m| m.into_iter().collect())
                .unwrap_or_default();
            let host = host
                .or_else(|| config.preview.as_ref().and_then(|p| p.host.clone()))
                .or_else(|| config.server.as_ref().and_then(|s| s.host.clone()));
            oj_server::preview(out_dir, port, base, headers, host).await
        }
    }
}
