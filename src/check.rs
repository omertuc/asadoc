//! `asadoc check` and `asadoc fix`, printed: the reports `report` builds, as
//! text for people and agents.

use std::env;
use std::io::{self, IsTerminal};

use similar::TextDiff;

use crate::config::AsadocConfig;
use crate::eval::Evaluation;
use crate::repo::{self, MarkerProblem};
use crate::report::{
    self, AwaitingFix, CheckOutcome, CheckSummary, Closest, FixOutcome, LineDiff, Sides, UnresolvedBlock,
    UnresolvedInAssembly, UnusedCode,
};
use anyhow::{Context, Result};

pub(crate) fn plural(count: usize, singular: &str, plural_form: &str) -> String {
    format!("{count} {}", if count == 1 { singular } else { plural_form })
}

/// A line difference, in words
pub(crate) fn describe_line_diff(line_diff: LineDiff) -> String {
    let doc_line_count_phrase = plural(line_diff.doc_line_count, "line", "lines");
    match (
        line_diff.differing_lines,
        line_diff.extra_code_lines.saturating_sub(line_diff.differing_lines),
    ) {
        (0, 0) => "same lines, but not the same text (line endings or placeholders)".to_owned(),
        (0, more_code_lines) => format!("the code has {} more", plural(more_code_lines, "line", "lines")),
        (differing_lines, 0) => format!("{differing_lines} of {doc_line_count_phrase} differ"),
        (differing_lines, more_code_lines) => {
            format!("{differing_lines} of {doc_line_count_phrase} differ, and the code has {more_code_lines} more")
        }
    }
}

fn describe_closest(closest_code: &Closest) -> String {
    match closest_code {
        Closest::Marked { name, line_diff } => format!("{name} (marked; {})", describe_line_diff(*line_diff)),
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

pub(crate) fn print_summary(summary: &CheckSummary) {
    let blocks_to_resolve = summary.blocks_to_resolve();
    match summary.docs_descriptions.as_slice() {
        [docs_description] => println!("Docs: {docs_description}"),
        docs_descriptions => {
            println!("Docs:");
            for docs_description in docs_descriptions {
                println!("  {docs_description}");
            }
        }
    }
    if !summary.other_code_descriptions.is_empty() {
        println!("Code: this repo, and");
        for code_description in &summary.other_code_descriptions {
            println!("  {code_description}");
        }
    }
    let code_where = if summary.other_code_descriptions.is_empty() {
        " in this repo"
    } else {
        ""
    };
    let awaiting = if summary.awaiting_blocks > 0 {
        format!(" {} a doc fix,", plural(summary.awaiting_blocks, "awaits", "await"))
    } else {
        String::new()
    };
    println!(
        "{}: {} match marked code{code_where}, {} are ignored,{awaiting} {blocks_to_resolve} {} still to resolve.",
        plural(summary.total_blocks, "doc code block", "doc code blocks"),
        summary.resolved_blocks,
        summary.ignored_blocks,
        if blocks_to_resolve == 1 { "is" } else { "are" }
    );
    summary.unresolved.iter().for_each(print_unresolved);
    print_awaiting(&summary.awaiting);
    print_unused(&summary.unused_code);
    print_stale_ignored(&summary.stale_ignored);
    print_stale_awaiting(&summary.stale_awaiting);
    print_problems(&summary.problems);
    print_next_steps(summary);
}

fn print_unresolved(assembly: &UnresolvedInAssembly) {
    print_heading(&assembly.title, &assembly.path);
    for block in &assembly.blocks {
        print_block("✗", block, 2);
    }
}

/// A block no code matches, `depth` spaces in: where it is, and its closest code
fn print_block(mark: &str, block: &UnresolvedBlock, depth: usize) {
    println!("{}{mark} {}", " ".repeat(depth), block.reference);
    let indent = " ".repeat(depth + 4);
    println!("{indent}doc:     {}", block.location);
    match &block.closest {
        Some(closest_code) => println!("{indent}closest: {}", describe_closest(closest_code)),
        None => println!("{indent}closest: no marked code resembles it"),
    }
    if let Some(fix_steps) = &block.fix_steps {
        println!(
            "{indent}fix:     `asadoc fix {}` would {}",
            block.reference,
            fix_steps.join(", then ")
        );
    }
}

fn print_awaiting(awaiting: &[AwaitingFix]) {
    if awaiting.is_empty() {
        return;
    }
    print_heading("Out of date, awaiting a doc fix", "");
    for fix in awaiting {
        println!("  {}", fix.name);
        for line in fix.description.lines() {
            println!("    {line}");
        }
        for block in &fix.blocks {
            print_block("~", block, 4);
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

fn print_stale_awaiting(stale_awaiting: &[(String, String)]) {
    if stale_awaiting.is_empty() {
        return;
    }
    print_heading("Awaiting a doc fix, but no doc block needs it anymore", "");
    for (fix, first_line) in stale_awaiting {
        println!("  - {fix}: {first_line}");
    }
    println!(
        "  The docs changed, or the code matches them again: delete its files from the awaiting-doc-fix directory."
    );
}

fn print_problems(problems: &[MarkerProblem]) {
    if problems.is_empty() {
        return;
    }
    print_heading("Markers that can't be read", "");
    for problem in problems {
        println!("  ! {}: {}", problem.file, problem.message);
    }
}

/// `asadoc todo`: every `TODO` on this repo's markers
pub(crate) fn list_todos(config: &AsadocConfig) -> Result<bool> {
    let scan = repo::scan(config).context("scanning the code for markers")?;
    for todo in &scan.todos {
        println!("{}:{}: {}", todo.file, todo.line, todo.text);
    }
    Ok(true)
}

/// The all-clear, or what to do next
fn print_next_steps(summary: &CheckSummary) {
    if summary.ok() {
        if summary.awaiting_blocks > 0 {
            println!("\n✓ Every doc code block matches marked code, is ignored, or awaits a doc fix.");
        } else {
            println!("\n✓ Every doc code block matches marked code or is ignored.");
        }
        return;
    }
    println!();
    if summary.blocks_to_resolve() > 0 {
        println!(
            "Resolve each ✗ block: make repo code match it, ignore it if it doesn't come from this repo, or, if \
             the docs are out of date, have it await a doc fix."
        );
    }
    println!("  asadoc check <block>   how a block differs from its closest code");
    println!("  asadoc check <file>    how marked code differs from its closest doc block");
    if summary.fixable_blocks() > 0 {
        println!("  asadoc fix <block>     make the change listed under the block");
    }
    if summary.blocks_to_resolve() > 0 {
        println!("  asadoc await-doc-fix <block> --fix <name> --description <text>");
        println!("                         have it await a fix to the docs");
    }
    println!("  asadoc guide           how markers, ignoring and awaiting doc fixes work");
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
        CheckOutcome::AwaitingDocFix {
            reference,
            location,
            fix,
            closest,
        } => match closest {
            Some((closest_code, sides)) => {
                println!(
                    "~ {reference} ({location}): awaiting the doc fix {fix}; until then the closest code is {}",
                    describe_closest(closest_code)
                );
                print_diff(sides);
            }
            None => println!("~ {reference} ({location}): awaiting the doc fix {fix}; no marked code resembles it"),
        },
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
            println!("✗ {reference} ({location}): no marked code matches or resembles it. The block:");
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

/// `asadoc await-doc-fix <blocks> --fix <name>`
pub(crate) fn await_doc_fix(
    config: &AsadocConfig,
    blocks: &[String],
    fix: &str,
    description: Option<&str>,
) -> Result<bool> {
    let working_dir = env::current_dir().context("finding the working directory")?;
    for (reference, path) in report::await_doc_fix(config, blocks, fix, description)? {
        let shown_path = path.strip_prefix(&working_dir).unwrap_or(&path);
        println!("~ {reference}: awaiting the doc fix {fix} ({})", shown_path.display());
    }
    Ok(true)
}
