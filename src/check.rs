//! `asadoc check` and `asadoc fix`, printed: the reports `report` builds, as
//! text for people and agents.

use std::env;
use std::io::{self, IsTerminal};

use similar::TextDiff;

use crate::config::AsadocConfig;
use crate::eval::Evaluation;
use crate::repo::Problem;
use crate::report::{
    self, CheckOutcome, CheckSummary, Closest, FixOutcome, LineDiff, Sides, UnresolvedInAssembly, UnusedCode,
};
use anyhow::{Context, Result};

fn plural(count: usize, singular: &str, plural_form: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural_form })
}

/// A line difference, in words
fn describe_line_diff(line_diff: LineDiff) -> String {
    let doc_lines_in_words = plural(line_diff.doc_line_count, "line", "lines");
    match (
        line_diff.differing_lines,
        line_diff.extra_code_lines.saturating_sub(line_diff.differing_lines),
    ) {
        (0, 0) => "same lines, but not the same text (line endings or placeholders)".to_owned(),
        (0, more_code_lines) => format!("the code has {} more", plural(more_code_lines, "line", "lines")),
        (differing_lines, 0) => format!("{differing_lines} of {doc_lines_in_words} differ"),
        (differing_lines, more_code_lines) => {
            format!("{differing_lines} of {doc_lines_in_words} differ, and the code has {more_code_lines} more")
        }
    }
}

fn describe_closest(closest_code: &Closest) -> String {
    match closest_code {
        Closest::Marked { name, line_diff } => format!("{name} (marked; {})", describe_line_diff(*line_diff)),
        Closest::UnmarkedFile { file, line_diff } => format!(
            "{file} (not marked; {} of the block's {} lines aren't in it)",
            line_diff.differing_lines, line_diff.doc_line_count
        ),
        Closest::Lines {
            file,
            first_line,
            last_line,
        } => format!("{file}, lines {first_line}-{last_line} (not marked)"),
    }
}

/// Bold and underlined when printing to a terminal
fn print_heading(title: &str, aside: &str) {
    let use_terminal_style = io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none();
    let aside = if aside.is_empty() {
        String::new()
    } else {
        format!("  ({aside})")
    };
    if use_terminal_style {
        println!("\n\x1b[1;4m{title}\x1b[0m{aside}");
    } else {
        println!("\n{title}{aside}\n{}", "─".repeat(title.chars().count()));
    }
}

fn print_diff(sides: &Sides) {
    let line_count = |text: &str| plural(text.lines().count(), "line", "lines");
    let doc_label = format!("{}, {}", sides.doc_label, line_count(&sides.doc_text));
    let code_label = format!("{}, {}", sides.code_label, line_count(&sides.code_text));
    let text_diff = TextDiff::from_lines(&sides.doc_text, &sides.code_text);
    print!(
        "{}",
        text_diff
            .unified_diff()
            .context_radius(3)
            .header(&doc_label, &code_label)
    );
}

/// `asadoc check`: the whole picture; false when anything needs attention
pub(crate) fn check_all(config: &AsadocConfig, evaluation: &Evaluation) -> Result<bool> {
    let summary = CheckSummary::build(config, evaluation).context("summarizing the evaluation")?;
    print_summary(&summary);
    Ok(summary.ok())
}

fn print_summary(summary: &CheckSummary) {
    let blocks_to_resolve = summary.blocks_to_resolve();
    println!("Docs: {}", summary.docs_description);
    println!(
        "{}: {} match marked code in this repo, {} are ignored, {blocks_to_resolve} {} still to resolve.",
        plural(summary.total_blocks, "doc code block", "doc code blocks"),
        summary.resolved_blocks,
        summary.ignored_blocks,
        if blocks_to_resolve == 1 { "is" } else { "are" }
    );
    summary.unresolved.iter().for_each(print_unresolved);
    print_unused(&summary.unused_code);
    print_stale_ignored(&summary.stale_ignored);
    print_problems(&summary.problems);
    print_next_steps(summary);
}

fn print_unresolved(assembly: &UnresolvedInAssembly) {
    print_heading(&assembly.title, &assembly.path);
    for block in &assembly.blocks {
        println!("  ✗ {}", block.reference);
        println!("      doc:     {}", block.location);
        match &block.closest {
            Some(closest_code) => println!("      closest: {}", describe_closest(closest_code)),
            None => println!("      closest: nothing in the repo resembles it"),
        }
        if let Some(fix_steps) = &block.fix_steps {
            println!(
                "      fix:     `asadoc fix {}` would {}",
                block.reference,
                fix_steps.join(", then ")
            );
        }
    }
}

fn print_unused(unused_code: &[UnusedCode]) {
    if unused_code.is_empty() {
        return;
    }
    print_heading("Marked code that no doc block matches", "");
    for marked_code in unused_code {
        println!("  ! {}", marked_code.name);
        match &marked_code.closest_block {
            Some((reference, line_diff)) => println!(
                "      closest doc block: {reference} ({})",
                describe_line_diff(*line_diff)
            ),
            None => println!("      closest doc block: none resembles it"),
        }
    }
    println!("  If the docs no longer show it, remove its markers.");
}

fn print_stale_ignored(stale_ignored: &[(String, String)]) {
    if stale_ignored.is_empty() {
        return;
    }
    print_heading("Ignored content that no doc block has anymore", "");
    for (reason, first_line) in stale_ignored {
        println!("  - {reason}: {first_line}");
    }
    println!("  Delete its files from the ignore directory, or remove it in `asadoc serve`.");
}

fn print_problems(problems: &[Problem]) {
    if problems.is_empty() {
        return;
    }
    print_heading("Markers that can't be read", "");
    for problem in problems {
        println!("  ! {}: {}", problem.file, problem.message);
    }
}

/// The all-clear, or what to do next
fn print_next_steps(summary: &CheckSummary) {
    if summary.ok() {
        println!("\n✓ Every doc code block matches marked code or is ignored.");
        return;
    }
    println!();
    if summary.blocks_to_resolve() > 0 {
        println!("Resolve each ✗ block: make repo code match it, or ignore it if it doesn't come from this repo.");
    }
    println!("  asadoc check <block>   how a block differs from its closest code");
    println!("  asadoc check <file>    how marked code differs from its closest doc block");
    if summary.fixable_blocks() > 0 {
        println!("  asadoc fix <block>     make the change listed under the block");
    }
    println!("  asadoc guide           how markers and ignoring work");
}

/// `asadoc check <names>`: false unless everything given is resolved or ignored
pub(crate) fn check_refs(evaluation: &Evaluation, names: &[String], against_arg: Option<&str>) -> Result<bool> {
    let outcomes = report::check_outcomes(evaluation, names, against_arg)?;
    outcomes.iter().for_each(print_outcome);
    Ok(outcomes.iter().all(CheckOutcome::ok))
}

fn print_outcome(outcome: &CheckOutcome) {
    match outcome {
        CheckOutcome::NotFound { name } => println!("✗ {name}: no doc block or marked code by that name"),
        CheckOutcome::Ignored { reference, reason } => println!("✓ {reference}: ignored ({reason})"),
        CheckOutcome::Resolved {
            reference,
            matched_codes,
            placeholder_values,
        } => {
            println!("✓ {reference}: resolved (matches {})", matched_codes.join("; "));
            for (placeholder, value) in placeholder_values {
                println!("    {placeholder} = {value:?}");
            }
        }
        CheckOutcome::Unmatched {
            reference,
            location,
            closest: closest_code,
            fix_steps,
            sides,
        } => {
            println!(
                "✗ {reference} ({location}): no marked code matches it; the closest is {}",
                describe_closest(closest_code)
            );
            if let Some(fix_steps) = fix_steps {
                println!("  `asadoc fix {reference}` would {}", fix_steps.join(", then "));
            }
            print_diff(sides);
        }
        CheckOutcome::NothingResembles {
            reference,
            location,
            block_content,
        } => {
            println!(
                "✗ {reference} ({location}): no marked code matches it, and nothing in the repo resembles it. The block:"
            );
            print!("----\n{block_content}----\n");
        }
        CheckOutcome::Mismatch {
            reference,
            location,
            against_code,
            sides,
        } => {
            println!("✗ {reference} ({location}): {against_code} doesn't match it");
            print_diff(sides);
        }
        CheckOutcome::DocOptionsDontFit {
            reference,
            against_code,
        } => {
            println!("✗ {reference}: the doc options of {against_code} don't fit this block");
        }
        CheckOutcome::CodeMatches { name, matched_blocks } => {
            println!("✓ {name}: matches {}", matched_blocks.join(", "));
        }
        CheckOutcome::CodeUnmatched {
            name,
            closest_block: None,
        } => {
            println!("✗ {name}: no doc block resembles it; if the docs no longer show it, remove its markers");
        }
        CheckOutcome::CodeUnmatched {
            name,
            closest_block: Some((reference, location, sides)),
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
        FixOutcome::Applied {
            changed_file,
            steps,
            resolved,
        } => {
            for step in steps {
                println!("  {step}");
            }
            if *resolved {
                println!("✓ {reference}: resolved");
            } else {
                println!(
                    "✗ {reference}: changed {changed_file}, but it still doesn't match; see `asadoc check {reference}`"
                );
            }
        }
    }
    Ok(outcome.ok())
}
