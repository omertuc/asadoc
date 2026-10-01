#[macro_use]
mod re;

mod awaiting;
mod check;
mod config;
mod docs;
mod eval;
mod github;
mod ignored;
mod lightbulb;
mod links;
mod markers;
mod matching;
mod progress;
mod repo;
mod report;
mod server;
mod source;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

/// Keeps docs code blocks and repo code in sync, through comment markers in the code.
#[derive(Parser)]
#[command(version)]
struct AsadocCli {
    /// The config file (default: the nearest .asadoc/config.toml from here up)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// A local docs checkout to read instead of a configured docs source:
    /// `<name>=<dir>` (repeatable)
    #[arg(long, global = true, value_name = "NAME=DIR")]
    docs: Vec<String>,
    /// A local checkout to read instead of the `[[code]]` named NAME (repeatable)
    #[arg(long, global = true, value_name = "NAME=DIR")]
    code: Vec<String>,
    #[command(subcommand)]
    command: AsadocCommand,
}

#[derive(Subcommand)]
enum AsadocCommand {
    /// Report doc blocks still to resolve, unmatched marked code and marker
    /// problems (exits 1 if there are any). Given doc blocks or marked code
    /// (`path`, or `path, section "name"`), check just those, with a diff
    /// against their closest match when they don't match
    Check {
        refs: Vec<String>,
        /// Compare the given doc blocks with this marked code instead of their closest
        #[arg(long, value_name = "CODE")]
        against: Option<String>,
        /// How to report checking everything: `markdown` prints a report for
        /// a GitHub job summary or PR comment; `github`, in GitHub Actions,
        /// also annotates the code and adds the report to the job summary
        #[arg(long, value_enum, default_value_t = CheckFormat::Text)]
        format: CheckFormat,
    },
    /// Make the change `asadoc check` lists under a doc block (marking lines
    /// of a file that has markers, and similar simple changes)
    Fix { block: String },
    /// Have doc blocks that are out of date (the code changed, the docs are
    /// still to follow) await a fix to the docs: `asadoc check` passes with
    /// them, and lists them, until their content changes. The fix is a
    /// directory in `.asadoc/awaiting-doc-fix/`, its README.md saying what the
    /// docs need to change
    AwaitDocFix {
        #[arg(required = true)]
        blocks: Vec<String>,
        /// The doc fix's name: lowercase letters, digits and dashes
        #[arg(long)]
        fix: String,
        /// For a new doc fix: what the docs need to change, and where that
        /// change is tracked (an issue or PR)
        #[arg(long)]
        description: Option<String>,
    },
    /// List the `TODO`s on this repo's markers, as `file:line: text`
    Todo,
    /// Serve the review UI
    Serve {
        #[arg(long, default_value_t = 3000)]
        port: u16,
    },
    /// How Asadoc works: markers, their options, ignoring, awaiting doc fixes, checking
    Guide,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CheckFormat {
    Text,
    Markdown,
    Github,
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
    let cli = AsadocCli::parse();
    let load_config =
        || config::AsadocConfig::load(cli.config.as_deref(), &cli.docs, &cli.code).context("loading the config");
    match &cli.command {
        AsadocCommand::Guide => {
            print!("{}", include_str!("../GUIDE.md"));
            Ok(true)
        }
        AsadocCommand::Check { refs, against, format } => {
            if !refs.is_empty() && *format != CheckFormat::Text {
                bail!("--format markdown and --format github are for checking everything; give no blocks or code");
            }
            let config = load_config()?;
            let evaluation = eval::evaluate(&config, true).context("evaluating the doc blocks")?;
            match format {
                _ if !refs.is_empty() => {
                    check::check_refs(&evaluation, refs, against.as_deref()).context("checking what was given")
                }
                CheckFormat::Text => check::check_all(&config, &evaluation).context("checking every doc block"),
                CheckFormat::Markdown => {
                    github::print_markdown(&config, &evaluation).context("checking every doc block")
                }
                CheckFormat::Github => github::report(&config, &evaluation).context("checking every doc block"),
            }
        }
        AsadocCommand::AwaitDocFix {
            blocks,
            fix,
            description,
        } => check::await_doc_fix(&load_config()?, blocks, fix, description.as_deref())
            .with_context(|| format!("making the blocks await the doc fix {fix}")),
        AsadocCommand::Todo => check::list_todos(&load_config()?).context("listing the TODOs"),
        AsadocCommand::Fix { block } => check::fix(&load_config()?, block).with_context(|| format!("fixing {block}")),
        AsadocCommand::Serve { port } => {
            let (config_path, docs, code) = (cli.config.clone(), cli.docs.clone(), cli.code.clone());
            let load_config =
                move || config::AsadocConfig::load(config_path.as_deref(), &docs, &code).context("loading the config");
            server::serve(load_config, *port).context("serving the review UI")?;
            Ok(true)
        }
    }
}
