//! `asadoc check` and `asadoc fix`, printed: the reports `report` builds, as
//! text for people and agents.

use std::env;
use std::io::{self, IsTerminal};

use similar::TextDiff;

use crate::config::AsadocConfig;
use crate::eval::Evaluation;
use crate::repo::Problem;
use crate::report::{self, Closest, FixOutcome, LineDiff, Outcome, Sides, Summary, UnresolvedInAssembly, Unused};
use anyhow::{Context, Result};

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A line difference, in words
fn line_diff(diff: LineDiff) -> String {
    let total = plural(diff.doc_lines, "line", "lines");
    match (diff.differ, diff.extra.saturating_sub(diff.differ)) {
        (0, 0) => "same lines, but not the same text (line endings or placeholders)".to_owned(),
        (0, more) => format!("the code has {} more", plural(more, "line", "lines")),
        (differ, 0) => format!("{differ} of {total} differ"),
        (differ, more) => format!("{differ} of {total} differ, and the code has {more} more"),
    }
}

fn closest(closest_code: &Closest) -> String {
    match closest_code {
        Closest::Marked { name, diff } => format!("{name} (marked; {})", line_diff(*diff)),
        Closest::UnmarkedFile { file, diff } => format!(
            "{file} (not marked; {} of the block's {} lines aren't in it)",
            diff.differ, diff.doc_lines
        ),
        Closest::Lines { file, from, to } => format!("{file}, lines {from}-{to} (not marked)"),
    }
}

/// Bold and underlined when printing to a terminal
fn heading(title: &str, aside: &str) {
    let styled = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
    let aside = if aside.is_empty() {
        String::new()
    } else {
        format!("  ({aside})")
    };
    if styled {
        println!("\n\x1b[1;4m{title}\x1b[0m{aside}");
    } else {
        println!("\n{title}{aside}\n{}", "─".repeat(title.chars().count()));
    }
}

fn print_diff(sides: &Sides) {
    let lines = |text: &str| plural(text.lines().count(), "line", "lines");
    let doc_label = format!("{}, {}", sides.doc_label, lines(&sides.doc));
    let code_label = format!("{}, {}", sides.code_label, lines(&sides.code));
    let diff = TextDiff::from_lines(&sides.doc, &sides.code);
    print!(
        "{}",
        diff.unified_diff().context_radius(3).header(&doc_label, &code_label)
    );
}

/// `asadoc check`: the whole picture; false when anything needs attention
pub(crate) fn check_all(config: &AsadocConfig, evaluation: &Evaluation) -> Result<bool> {
    let summary = Summary::build(config, evaluation).context("summarizing the evaluation")?;
    print_summary(&summary);
    Ok(summary.ok())
}

fn print_summary(summary: &Summary) {
    let open = summary.open();
    println!("Docs: {}", summary.docs);
    println!(
        "{}: {} match marked code in this repo, {} are ignored, {open} {} still to resolve.",
        plural(summary.total, "doc code block", "doc code blocks"),
        summary.resolved,
        summary.ignored,
        if open == 1 { "is" } else { "are" }
    );
    summary.unresolved.iter().for_each(print_unresolved);
    print_unused(&summary.unused);
    print_stale_ignored(&summary.stale_ignored);
    print_problems(&summary.problems);
    print_next_steps(summary);
}

fn print_unresolved(assembly: &UnresolvedInAssembly) {
    heading(&assembly.title, &assembly.path);
    for block in &assembly.blocks {
        println!("  ✗ {}", block.reference);
        println!("      doc:     {}", block.location);
        match &block.closest {
            Some(closest_code) => println!("      closest: {}", closest(closest_code)),
            None => println!("      closest: nothing in the repo resembles it"),
        }
        if let Some(steps) = &block.fix {
            println!(
                "      fix:     `asadoc fix {}` would {}",
                block.reference,
                steps.join(", then ")
            );
        }
    }
}

fn print_unused(unused: &[Unused]) {
    if unused.is_empty() {
        return;
    }
    heading("Marked code that no doc block matches", "");
    for code in unused {
        println!("  ! {}", code.name);
        match &code.closest {
            Some((reference, diff)) => println!("      closest doc block: {reference} ({})", line_diff(*diff)),
            None => println!("      closest doc block: none resembles it"),
        }
    }
    println!("  If the docs no longer show it, remove its markers.");
}

fn print_stale_ignored(stale_ignored: &[(String, String)]) {
    if stale_ignored.is_empty() {
        return;
    }
    heading("Ignored content that no doc block has anymore", "");
    for (reason, first_line) in stale_ignored {
        println!("  - {reason}: {first_line}");
    }
    println!("  Delete its files from the ignore directory, or remove it in `asadoc serve`.");
}

fn print_problems(problems: &[Problem]) {
    if problems.is_empty() {
        return;
    }
    heading("Markers that can't be read", "");
    for problem in problems {
        println!("  ! {}: {}", problem.file, problem.message);
    }
}

/// The all-clear, or what to do next
fn print_next_steps(summary: &Summary) {
    if summary.ok() {
        println!("\n✓ Every doc code block matches marked code or is ignored.");
        return;
    }
    println!();
    if summary.open() > 0 {
        println!("Resolve each ✗ block: make repo code match it, or ignore it if it doesn't come from this repo.");
    }
    println!("  asadoc check <block>   how a block differs from its closest code");
    println!("  asadoc check <file>    how marked code differs from its closest doc block");
    if summary.fixable() > 0 {
        println!("  asadoc fix <block>     make the change listed under the block");
    }
    println!("  asadoc guide           how markers and ignoring work");
}

/// `asadoc check <names>`: false unless everything given is resolved or ignored
pub(crate) fn check_refs(evaluation: &Evaluation, names: &[String], against: Option<&str>) -> Result<bool> {
    let outcomes = report::outcomes(evaluation, names, against)?;
    outcomes.iter().for_each(print_outcome);
    Ok(outcomes.iter().all(Outcome::ok))
}

fn print_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::NotFound { name } => println!("✗ {name}: no doc block or marked code by that name"),
        Outcome::Ignored { reference, reason } => println!("✓ {reference}: ignored ({reason})"),
        Outcome::Resolved {
            reference,
            codes,
            values,
        } => {
            println!("✓ {reference}: resolved (matches {})", codes.join("; "));
            for (placeholder, value) in values {
                println!("    {placeholder} = {value:?}");
            }
        }
        Outcome::Unmatched {
            reference,
            location,
            closest: closest_code,
            fix,
            sides,
        } => {
            println!(
                "✗ {reference} ({location}): no marked code matches it; the closest is {}",
                closest(closest_code)
            );
            if let Some(steps) = fix {
                println!("  `asadoc fix {reference}` would {}", steps.join(", then "));
            }
            print_diff(sides);
        }
        Outcome::Alone {
            reference,
            location,
            content,
        } => {
            println!(
                "✗ {reference} ({location}): no marked code matches it, and nothing in the repo resembles it. The block:"
            );
            print!("----\n{content}----\n");
        }
        Outcome::Mismatch {
            reference,
            location,
            code,
            sides,
        } => {
            println!("✗ {reference} ({location}): {code} doesn't match it");
            print_diff(sides);
        }
        Outcome::DocOptionsDontFit { reference, code } => {
            println!("✗ {reference}: the doc options of {code} don't fit this block");
        }
        Outcome::CodeMatches { name, blocks } => println!("✓ {name}: matches {}", blocks.join(", ")),
        Outcome::CodeUnmatched { name, closest: None } => {
            println!("✗ {name}: no doc block resembles it; if the docs no longer show it, remove its markers");
        }
        Outcome::CodeUnmatched {
            name,
            closest: Some((reference, location, sides)),
        } => {
            println!("✗ {name}: no doc block matches it; the closest is {reference} ({location})");
            print_diff(sides);
        }
    }
}

/// `asadoc fix <block>`
pub(crate) fn fix(config: &AsadocConfig, reference: &str) -> Result<bool> {
    let outcome = report::fix(config, reference)?;
    match &outcome {
        FixOutcome::AlreadyDone => println!("✓ {reference} is already resolved or ignored"),
        FixOutcome::NoFix => {
            println!("✗ {reference}: there's no simple fix for it; see how it differs with `asadoc check {reference}`");
        }
        FixOutcome::Applied { file, steps, resolved } => {
            for step in steps {
                println!("  {step}");
            }
            if *resolved {
                println!("✓ {reference}: resolved");
            } else {
                println!("✗ {reference}: changed {file}, but it still doesn't match; see `asadoc check {reference}`");
            }
        }
    }
    Ok(outcome.ok())
}
