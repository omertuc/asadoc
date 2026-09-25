mod check;
mod config;
mod docs;
mod eval;
mod ignored;
mod lightbulb;
mod markers;
mod matching;
mod repo;
mod server;
mod source;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

/// Keeps docs code blocks and repo code in sync, through comment markers in the code.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The config file (default: the nearest asadoc.yaml from here up)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// A local docs checkout, instead of the docs in the config
    #[arg(long, global = true)]
    docs: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report doc blocks still to resolve, unmatched marked code and marker
    /// problems (exits 1 if there are any); with refs, check just those blocks
    /// and show a diff for unresolved ones
    Check { refs: Vec<String> },
    /// Serve the review UI
    Serve {
        #[arg(long, default_value_t = 3000)]
        port: u16,
    },
    /// How asadoc works: markers, their options, ignoring, checking
    Guide,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("asadoc: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    if let Command::Guide = cli.command {
        print!("{}", include_str!("../GUIDE.md"));
        return Ok(true);
    }
    let config = config::Config::load(cli.config.as_deref(), cli.docs.as_deref())?;
    match cli.command {
        Command::Check { refs } if refs.is_empty() => Ok(check::check_all(&eval::evaluate(&config, true)?)),
        Command::Check { refs } => Ok(check::check_blocks(&eval::evaluate(&config, true)?, &refs)),
        Command::Serve { port } => {
            server::serve(config, port)?;
            Ok(true)
        }
        Command::Guide => unreachable!(),
    }
}
