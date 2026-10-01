//! `asadoc check --format markdown` and `--format github`: the report as
//! markdown (for a GitHub job summary or PR comment), and as GitHub workflow
//! annotations on the code it's about.

use std::env;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;

use similar::TextDiff;

use crate::check;
use crate::config::AsadocConfig;
use crate::docs;
use crate::eval::{AssemblyEval, BlockEval, CandidateInfo, Evaluation};
use crate::repo::MarkedCode;
use crate::report::{self, CheckOutcome, CheckSummary, Closest, LineDiff, Sides};
use anyhow::{Context, Result};

/// Diff lines shown per block; the rest are left to `asadoc check <block>`
const MAX_DIFF_LINES: usize = 150;

/// `asadoc check --format markdown`: the report as markdown, on stdout
pub(crate) fn print_markdown(config: &AsadocConfig, evaluation: &Evaluation) -> Result<bool> {
    let summary = CheckSummary::build(config, evaluation).context("summarizing the evaluation")?;
    print!("{}", markdown(config, evaluation, &summary)?);
    Ok(summary.ok())
}

/// `asadoc check --format github`: the text report and annotations on
/// stdout (the job log), and the markdown report added to the job summary
pub(crate) fn report(config: &AsadocConfig, evaluation: &Evaluation) -> Result<bool> {
    let summary = CheckSummary::build(config, evaluation).context("summarizing the evaluation")?;
    check::print_summary(&summary);
    print_annotations(config, evaluation, &summary);
    let summary_path = env::var_os("GITHUB_STEP_SUMMARY")
        .context("GITHUB_STEP_SUMMARY isn't set: `--format github` is for GitHub Actions; try `--format markdown`")?;
    let markdown = markdown(config, evaluation, &summary)?;
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(&summary_path)
        .and_then(|mut summary_file| summary_file.write_all(markdown.as_bytes()))
        .with_context(|| format!("writing the job summary to {}", summary_path.display()))?;
    Ok(summary.ok())
}

fn markdown(config: &AsadocConfig, evaluation: &Evaluation, summary: &CheckSummary) -> Result<String> {
    let mut out = String::new();
    let blocks_to_resolve = summary.blocks_to_resolve();
    if summary.ok() {
        writeln!(out, "## ✅ The docs' code blocks match this repo's code\n")?;
    } else if blocks_to_resolve > 0 {
        writeln!(
            out,
            "## ❌ {} in the docs {} match this repo's code\n",
            check::plural(blocks_to_resolve, "code block", "code blocks"),
            if blocks_to_resolve == 1 { "doesn't" } else { "don't" }
        )?;
    } else {
        writeln!(out, "## ❌ Some markers can't be read\n")?;
    }
    writeln!(
        out,
        "Code in this repo marked with `@code-as-a-doc:` comments has to match the code blocks in the docs, \
         so that the docs and the code don't drift apart. Every code block in the docs has to match marked code \
         or be ignored.\n"
    )?;
    for docs_description in &summary.docs_descriptions {
        writeln!(out, "Docs: {docs_description}  ")?;
    }
    writeln!(out, "\n| Code blocks | ✅ Match code | ➖ Ignored | ❌ To resolve |")?;
    writeln!(out, "|---:|---:|---:|---:|")?;
    writeln!(
        out,
        "| {} | {} | {} | {blocks_to_resolve} |\n",
        summary.total_blocks, summary.resolved_blocks, summary.ignored_blocks,
    )?;

    for assembly_eval in &evaluation.assemblies {
        write_assembly(&mut out, config, evaluation, assembly_eval)?;
    }
    write_unused(&mut out, config, evaluation, summary)?;
    write_stale_ignored(&mut out, summary)?;
    write_problems(&mut out, summary)?;
    if !summary.ok() {
        write_next_steps(&mut out, summary)?;
    }
    Ok(out)
}

/// An assembly's blocks still to resolve, if it has any
fn write_assembly(
    out: &mut String,
    config: &AsadocConfig,
    evaluation: &Evaluation,
    assembly_eval: &AssemblyEval,
) -> Result<()> {
    let unresolved_blocks: Vec<&BlockEval> = assembly_eval
        .blocks
        .iter()
        .filter(|block_eval| !block_eval.done())
        .collect();
    if unresolved_blocks.is_empty() {
        return Ok(());
    }
    let assembly = &assembly_eval.assembly;
    let docs_links = config
        .docs
        .get(assembly.docs_index)
        .and_then(|docs| docs.links.as_ref());
    let assembly_path = format!("`{}`", assembly.path);
    let assembly_path = docs_links.map_or_else(
        || assembly_path.clone(),
        |links| format!("[{assembly_path}]({})", links.file(&assembly.path, None)),
    );
    writeln!(out, "### {}\n", assembly.title)?;
    writeln!(out, "<sub>{assembly_path}</sub>\n")?;
    for block_eval in unresolved_blocks {
        let doc_link = docs_links
            .map(|links| links.source_line(&docs::module_path(&block_eval.block.module), block_eval.block.line));
        write_block(out, config, evaluation, block_eval, doc_link.as_deref())?;
    }
    Ok(())
}

/// One block still to resolve: what it is, its closest code, and a collapsed diff
fn write_block(
    out: &mut String,
    config: &AsadocConfig,
    evaluation: &Evaluation,
    block_eval: &BlockEval,
    doc_link: Option<&str>,
) -> Result<()> {
    let block = &block_eval.block;
    let reference = html_link(&format!("<code>{}</code>", escape_html(&block.reference)), doc_link);
    let closest = match block_eval.candidates.first() {
        Some(candidate) => format!("closest code: {}", describe_candidate(config, candidate)),
        None => "no marked code resembles it".to_owned(),
    };
    writeln!(out, "<details>\n<summary>❌ {reference} — {closest}</summary>\n")?;
    let context = [block.section.as_deref(), block.lead.as_deref()]
        .into_iter()
        .flatten()
        .map(|text| format!("*{}*", text.trim()))
        .collect::<Vec<_>>();
    if !context.is_empty() {
        writeln!(out, "> {}\n", context.join(" › "))?;
    }
    let outcome = report::check_outcomes(evaluation, std::slice::from_ref(&block.reference), None)?;
    for outcome in &outcome {
        match outcome {
            CheckOutcome::Unmatched { fix_steps, sides, .. } => {
                if let Some(fix_steps) = fix_steps {
                    writeln!(
                        out,
                        "`asadoc fix '{}'` would {}.\n",
                        block.reference,
                        fix_steps.join(", then ")
                    )?;
                }
                writeln!(out, "How the doc block (`-`) differs from the code (`+`):\n")?;
                write_diff(out, sides, &block.reference)?;
            }
            CheckOutcome::NothingResembles { block_content, .. } => {
                writeln!(out, "The doc block:\n")?;
                write_fenced(out, &block.language, block_content)?;
            }
            _ => {}
        }
    }
    writeln!(out, "</details>\n")?;
    Ok(())
}

/// The candidate, linked when its code source says how, with how far it is
/// from the block
fn describe_candidate(config: &AsadocConfig, candidate: &CandidateInfo) -> String {
    let name = report::code_name(&candidate.file, candidate.section_name.as_deref());
    let link = config.code_file(&candidate.file).ok().and_then(|(code_source, path)| {
        code_source
            .links
            .as_ref()
            .map(|links| links.file(path, candidate.line_range))
    });
    match report::closest_from_candidate(candidate) {
        Closest::Marked { line_diff, .. } => format!(
            "{}, {}",
            html_link(&format!("<code>{}</code>", escape_html(&name)), link.as_deref()),
            escape_html(&check::describe_line_diff(line_diff))
        ),
        Closest::Lines {
            file,
            first_line,
            last_line,
        } => format!(
            "{}, not marked",
            html_link(
                &format!("<code>{}</code>, lines {first_line}-{last_line}", escape_html(&file)),
                link.as_deref()
            )
        ),
    }
}

fn write_diff(out: &mut String, sides: &Sides, reference: &str) -> Result<()> {
    let line_count = |text: &str| check::plural(text.lines().count(), "line", "lines");
    let doc_label = format!("{}, {}", sides.doc_label, line_count(&sides.doc_text));
    let code_label = format!("{}, {}", sides.code_label, line_count(&sides.code_text));
    let diff = TextDiff::from_lines(&sides.doc_text, &sides.code_text)
        .unified_diff()
        .context_radius(3)
        .header(&doc_label, &code_label)
        .to_string();
    let diff_line_count = diff.lines().count();
    let shown: String = diff
        .lines()
        .take(MAX_DIFF_LINES)
        .flat_map(|line| [line, "\n"])
        .collect();
    write_fenced(out, "diff", &shown)?;
    if diff_line_count > MAX_DIFF_LINES {
        writeln!(
            out,
            "…and {} more; see them all with `asadoc check '{reference}'`.\n",
            check::plural(diff_line_count - MAX_DIFF_LINES, "line", "lines")
        )?;
    }
    Ok(())
}

/// A fenced code block, its fence longer than any backtick run in the text
fn write_fenced(out: &mut String, language: &str, text: &str) -> Result<()> {
    let longest_backtick_run = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_backtick_run.max(2) + 1);
    writeln!(out, "{fence}{language}\n{}\n{fence}\n", text.trim_end_matches('\n'))?;
    Ok(())
}

fn write_unused(
    out: &mut String,
    config: &AsadocConfig,
    evaluation: &Evaluation,
    summary: &CheckSummary,
) -> Result<()> {
    if summary.unused_code.is_empty() {
        return Ok(());
    }
    writeln!(out, "### ⚠️ Marked code that no doc block matches\n")?;
    for unused in &summary.unused_code {
        let link = find_marked(evaluation, &unused.name).and_then(|marked_code| {
            let (code_source, path) = config.code_file(&marked_code.file).ok()?;
            code_source
                .links
                .as_ref()
                .map(|links| links.file(path, marked_code.line_range))
        });
        let name = html_link(&format!("<code>{}</code>", escape_html(&unused.name)), link.as_deref());
        match &unused.closest_block {
            Some((reference, line_diff)) => writeln!(
                out,
                "- {name}: the closest doc block is `{reference}` ({})",
                check::describe_line_diff(*line_diff)
            )?,
            None => writeln!(out, "- {name}: no doc block resembles it")?,
        }
    }
    writeln!(out, "\nIf the docs no longer show it, remove its markers.\n")?;
    Ok(())
}

fn write_stale_ignored(out: &mut String, summary: &CheckSummary) -> Result<()> {
    if summary.stale_ignored.is_empty() {
        return Ok(());
    }
    writeln!(out, "### ⚠️ Ignored content that no doc block has anymore\n")?;
    for (reason, first_line) in &summary.stale_ignored {
        writeln!(out, "- `{reason}`: `{}`", first_line.replace('`', "'"))?;
    }
    writeln!(
        out,
        "\nDelete its files from the ignore directory, or remove it in `asadoc serve`.\n"
    )?;
    Ok(())
}

fn write_problems(out: &mut String, summary: &CheckSummary) -> Result<()> {
    if summary.problems.is_empty() {
        return Ok(());
    }
    writeln!(out, "### ❌ Markers that can't be read\n")?;
    for problem in &summary.problems {
        writeln!(out, "- `{}`: {}", problem.file, problem.message)?;
    }
    writeln!(out)?;
    Ok(())
}

fn write_next_steps(out: &mut String, summary: &CheckSummary) -> Result<()> {
    writeln!(out, "### How to fix this\n")?;
    if summary.blocks_to_resolve() > 0 {
        writeln!(
            out,
            "Resolve each ❌ block: make the repo code match it, or ignore it if it doesn't come from this repo. \
             If the code is right and the docs are wrong, the docs need a fix; until then, the block stays \
             unresolved.\n"
        )?;
    }
    writeln!(out, "Locally, with [asadoc](https://github.com/omertuc/asadoc):\n")?;
    writeln!(out, "| Command | What it does |\n|---|---|")?;
    writeln!(
        out,
        "| `asadoc check '<block>'` | How a block differs from its closest code |"
    )?;
    writeln!(
        out,
        "| `asadoc check '<file>'` | How marked code differs from its closest doc block |"
    )?;
    if summary.fixable_blocks() > 0 {
        writeln!(
            out,
            "| `asadoc fix '<block>'` | Make the change listed under the block |"
        )?;
    }
    writeln!(out, "| `asadoc serve` | Review every block in a web UI |")?;
    writeln!(out, "| `asadoc guide` | How markers and ignoring work |\n")?;
    Ok(())
}

/// Annotations for the job log: on the code closest to each block still to
/// resolve, on marked code no block matches, and on markers that can't be read
fn print_annotations(config: &AsadocConfig, evaluation: &Evaluation, summary: &CheckSummary) {
    for (_, block_eval) in evaluation.blocks().filter(|(_, block_eval)| !block_eval.done()) {
        let reference = &block_eval.block.reference;
        let Some(candidate) = block_eval.candidates.first() else {
            println!(
                "{}",
                annotation(
                    "error",
                    None,
                    "Doc block matches no code",
                    &format!("No marked code resembles the doc block {reference}. See the job summary.")
                )
            );
            continue;
        };
        let line_diff = LineDiff::new(&candidate.block_side, &candidate.content);
        let message = match candidate.kind {
            "lines" => format!(
                "These lines would match the doc block {reference} if marked as a section. \
                 See the job summary, or run `asadoc check '{reference}'`."
            ),
            _ => format!(
                "The doc block {reference} doesn't match this code ({}). \
                 See the job summary for the diff, or run `asadoc check '{reference}'`.",
                check::describe_line_diff(line_diff)
            ),
        };
        println!(
            "{}",
            annotation(
                "error",
                repo_location(config, &candidate.file, candidate.line_range, &candidate.marker_lines).as_ref(),
                "Doesn't match the docs",
                &message
            )
        );
    }
    for unused in &summary.unused_code {
        let Some(marked_code) = find_marked(evaluation, &unused.name) else {
            continue;
        };
        println!(
            "{}",
            annotation(
                "warning",
                repo_location(
                    config,
                    &marked_code.file,
                    marked_code.line_range,
                    &marked_code.marker_lines
                )
                .as_ref(),
                "No doc block matches this",
                "No doc block matches this marked code. If the docs no longer show it, remove its markers."
            )
        );
    }
    for problem in &summary.problems {
        let location = repo_location(config, &problem.file, None, &[]);
        println!(
            "{}",
            annotation("error", location.as_ref(), "Marker can't be read", &problem.message)
        );
    }
}

/// A file in this repo and lines in it, for an annotation; None for other
/// repos' files, which GitHub can't place
struct RepoLocation<'a> {
    path: &'a str,
    lines: Option<(usize, usize)>,
}

fn repo_location<'a>(
    config: &AsadocConfig,
    file: &'a str,
    line_range: Option<(usize, usize)>,
    marker_lines: &[usize],
) -> Option<RepoLocation<'a>> {
    let (code_source, path) = config.code_file(file).ok()?;
    code_source.name.is_none().then(|| RepoLocation {
        path,
        lines: line_range.or_else(|| marker_lines.first().map(|&line| (line, line))),
    })
}

/// A GitHub workflow command that annotates a file, or the run
fn annotation(level: &str, location: Option<&RepoLocation<'_>>, title: &str, message: &str) -> String {
    let mut properties = Vec::new();
    if let Some(location) = location {
        properties.push(format!("file={}", escape_property(location.path)));
        if let Some((first_line, last_line)) = location.lines {
            properties.push(format!("line={first_line}"));
            properties.push(format!("endLine={last_line}"));
        }
    }
    properties.push(format!("title={}", escape_property(title)));
    format!("::{level} {}::{}", properties.join(","), escape_data(message))
}

/// Marked code by the name the report gives it
fn find_marked<'a>(evaluation: &'a Evaluation, name: &str) -> Option<&'a MarkedCode> {
    evaluation
        .scan
        .marked
        .iter()
        .find(|marked_code| report::code_name(&marked_code.file, marked_code.section.as_deref()) == name)
}

fn html_link(html: &str, link: Option<&str>) -> String {
    match link {
        Some(link) => format!("<a href=\"{}\">{html}</a>", escape_html(link)),
        None => html.to_owned(),
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A workflow command's message
fn escape_data(text: &str) -> String {
    text.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

/// A workflow command's property value
fn escape_property(text: &str) -> String {
    escape_data(text).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
#[allow(clippy::panic_in_result_fn, reason = "assertions are how tests fail")]
mod tests {
    use super::*;

    #[test]
    fn fences_outgrow_backticks_in_the_text() -> Result<()> {
        let mut out = String::new();
        write_fenced(&mut out, "md", "a\n````\nb\n")?;
        assert_eq!(out, "`````md\na\n````\nb\n`````\n\n");
        Ok(())
    }

    #[test]
    fn annotations_escape_their_values() {
        let location = RepoLocation {
            path: "a,b:c.yaml",
            lines: Some((3, 5)),
        };
        assert_eq!(
            annotation("error", Some(&location), "T: t", "50%\nmore"),
            "::error file=a%2Cb%3Ac.yaml,line=3,endLine=5,title=T%3A t::50%25%0Amore"
        );
    }
}
