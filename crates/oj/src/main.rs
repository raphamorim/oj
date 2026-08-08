// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

mod build;

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
        #[arg(long, default_value_t = 5199)]
        port: u16,
        /// Full bundle mode: serve one registry-runtime chunk instead of
        /// native ESM modules (experimental)
        #[arg(long)]
        bundle: bool,
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
        /// Output directory, relative to the app root
        #[arg(long, default_value = "dist")]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Dev { root, port, bundle } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() { playground } else { PathBuf::from(".") }
            });
            oj_server::DevServer { root, port, bundle }.run().await
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
        Command::Build { root, out } => {
            let root = root.unwrap_or_else(|| {
                let playground = PathBuf::from("playground");
                if playground.join("index.html").is_file() { playground } else { PathBuf::from(".") }
            });
            build::build(root, out).await
        }
    }
}
