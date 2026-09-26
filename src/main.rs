#[macro_use]
mod re;

mod check;
mod config;
mod docs;
mod eval;
mod ignored;
mod lightbulb;
mod markers;
mod matching;
mod repo;
mod report;
mod server;
mod source;

use anyhow::{Context, Result};
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
    /// problems (exits 1 if there are any). Given doc blocks or marked code
    /// (`path`, or `path, section "name"`), check just those, with a diff
    /// against their closest match when they don't match
    Check {
        refs: Vec<String>,
        /// Compare the given doc blocks with this marked code instead of their closest
        #[arg(long, value_name = "CODE")]
        against: Option<String>,
    },
    /// Make the change `asadoc check` lists under a doc block (marking a file
    /// or lines of one, and similar simple changes)
    Fix { block: String },
    /// Serve the review UI
    Serve {
        #[arg(long, default_value_t = 3000)]
        port: u16,
    },
    /// How Asadoc works: markers, their options, ignoring, checking
    Guide,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("asadoc: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    let load = || config::AsadocConfig::load(cli.config.as_deref(), cli.docs.as_deref()).context("loading the config");
    match &cli.command {
        Command::Guide => {
            print!("{}", include_str!("../GUIDE.md"));
            Ok(true)
        }
        Command::Check { refs, against } => {
            let config = load()?;
            let evaluation = eval::evaluate(&config, true).context("evaluating the doc blocks")?;
            if refs.is_empty() {
                check::check_all(&config, &evaluation).context("checking every doc block")
            } else {
                check::check_refs(&evaluation, refs, against.as_deref()).context("checking what was given")
            }
        }
        Command::Fix { block } => check::fix(&load()?, block).with_context(|| format!("fixing {block}")),
        Command::Serve { port } => {
            server::serve(load()?, *port).context("serving the review UI")?;
            Ok(true)
        }
    }
}
