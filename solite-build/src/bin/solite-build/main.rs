//! The `solite-build` command-line tool.
//!
//! - `solite-build init <dir>` scaffolds a new project (index.tsx, styles.css,
//!   tsconfig.json, runtime.d.ts).
//! - `solite-build bundle <src-dir> <out.rs>` transpiles a project ahead of time
//!   into a single Rust source file of virtual modules, so apps can ship a single
//!   executable with no sidecar source directory and no per-launch compilation.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod bundle;
mod init;

#[derive(Parser)]
#[command(
    name = "solite-build",
    about = "Scaffold and bundle solite apps",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a new solite project into a directory.
    Init {
        /// Directory to scaffold into (defaults to the current directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Scaffold even if the directory already contains files.
        #[arg(long)]
        force: bool,
    },
    /// Transpile a project ahead of time into a single Rust module file.
    Bundle {
        /// Source directory containing the entry (index.tsx/app.tsx) and imports.
        src_dir: PathBuf,
        /// Output `.rs` file to generate.
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result: Result<(), Box<dyn Error>> = match cli.command {
        Command::Init { dir, force } => init::run(&dir, force).map_err(Into::into),
        Command::Bundle { src_dir, out } => bundle::run(&src_dir, &out).map_err(Into::into),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
