//! What `asadoc check` and `asadoc fix` report, as data: built from an
//! evaluation here, printed by `check`.

use std::collections::HashSet;

use similar::{ChangeTag, TextDiff};

use crate::config::AsadocConfig;
use crate::eval::{self, AssemblyEval, BlockEval, CandidateInfo, CodeMatch, Evaluation};
use crate::lightbulb;
use crate::matching::Values;
use crate::repo::{MarkedCode, Problem};
use anyhow::{Context, Result, bail};

/// How far some code is from a doc block, in lines
#[derive(Debug, Clone, Copy)]
pub(crate) struct LineDiff {
    pub doc_line_count: usize,
    /// Doc lines the code doesn't have
    pub differing_lines: usize,
    /// Code lines the doc doesn't have
    pub extra_code_lines: usize,
}

impl LineDiff {
    pub(crate) fn new(doc_text: &str, code_text: &str) -> Self {
        let (differing_lines, extra_code_lines) = TextDiff::from_lines(doc_text, code_text).iter_all_changes().fold(
            (0, 0),
            |(differing_lines, extra_code_lines), change| match change.tag() {
                ChangeTag::Delete => (differing_lines + 1, extra_code_lines),
                ChangeTag::Insert => (differing_lines, extra_code_lines + 1),
                ChangeTag::Equal => (differing_lines, extra_code_lines),
            },
        );
        Self {
            doc_line_count: doc_text.lines().count(),
            differing_lines,
            extra_code_lines,
        }
    }
}

/// A doc side and a code side, to show as a diff
#[derive(Debug)]
pub(crate) struct Sides {
    pub doc_text: String,
    pub doc_label: String,
    pub code_text: String,
    pub code_label: String,
}

/// The repo code most like a doc block
#[derive(Debug)]
pub(crate) enum Closest {
    Marked {
        name: String,
        line_diff: LineDiff,
    },
    UnmarkedFile {
        file: String,
        line_diff: LineDiff,
    },
    /// Lines of a file that, marked as a section, would match
    Lines {
        file: String,
        first_line: usize,
        last_line: usize,
    },
}

/// A doc block still to resolve
#[derive(Debug)]
pub(crate) struct UnresolvedBlock {
    pub reference: String,
    pub location: String,
    pub closest: Option<Closest>,
    /// The changes `asadoc fix` would make
    pub fix_steps: Option<Vec<String>>,
}

/// The blocks still to resolve in one assembly
#[derive(Debug)]
pub(crate) struct UnresolvedInAssembly {
    pub title: String,
    pub path: String,
    pub blocks: Vec<UnresolvedBlock>,
}

/// Marked code no doc block matches
#[derive(Debug)]
pub(crate) struct UnusedCode {
    pub name: String,
    /// The block most like it
    pub closest_block: Option<(String, LineDiff)>,
}

/// Everything `asadoc check` reports
#[derive(Debug)]
pub(crate) struct CheckSummary {
    pub docs_description: String,
    pub total_blocks: usize,
    pub resolved_blocks: usize,
    pub ignored_blocks: usize,
    /// Blocks to resolve, by assembly
    pub unresolved: Vec<UnresolvedInAssembly>,
    /// Marked code no block matches that isn't any block's closest code
    pub unused_code: Vec<UnusedCode>,
    /// (reason, first line) of ignored content no block has anymore
    pub stale_ignored: Vec<(String, String)>,
    pub problems: Vec<Problem>,
}

impl CheckSummary {
    pub(crate) const fn blocks_to_resolve(&self) -> usize {
        self.total_blocks
            .saturating_sub(self.resolved_blocks + self.ignored_blocks)
    }
    pub(crate) fn fixable_blocks(&self) -> usize {
        self.unresolved
            .iter()
            .flat_map(|assembly| &assembly.blocks)
            .filter(|block| block.fix_steps.is_some())
            .count()
    }
    pub(crate) const fn ok(&self) -> bool {
        self.blocks_to_resolve() == 0 && self.problems.is_empty()
    }
}

/// How marked code is named to people, and on the command line
pub(crate) fn code_name(file: &str, section: Option<&str>) -> String {
    match section {
        Some(section) => format!("{file}, section \"{section}\""),
        None => file.to_owned(),
    }
}

fn describe_code(marked_code: &MarkedCode) -> String {
    code_name(&marked_code.file, marked_code.section.as_deref())
}

fn block_location(block_eval: &BlockEval) -> String {
    format!("modules/{}.adoc:{}", block_eval.block.module, block_eval.block.line)
}

fn closest_from_candidate(candidate: &CandidateInfo) -> Closest {
    match candidate.kind {
        "unmarked-file" => Closest::UnmarkedFile {
            file: candidate.file.clone(),
            line_diff: LineDiff::new(&candidate.doc, &candidate.content),
        },
        "lines" => {
            let (first_line, last_line) = candidate.lines.unwrap_or_default();
            Closest::Lines {
                file: candidate.file.clone(),
                first_line,
                last_line,
            }
        }
        _ => Closest::Marked {
            name: code_name(&candidate.file, candidate.name.as_deref()),
            line_diff: LineDiff::new(&candidate.doc, &candidate.content),
        },
    }
}

fn fix_steps(candidate: &CandidateInfo) -> Option<Vec<String>> {
    candidate
        .plan
        .as_ref()
        .map(|plan| lightbulb::describe(plan, &candidate.file, candidate.name.as_deref()))
}

fn doc_label(block_eval: &BlockEval, doc_options_applied: bool) -> String {
    format!(
        "doc: {}{}",
        block_eval.block.reference,
        if doc_options_applied {
            " (doc options applied)"
        } else {
            ""
        }
    )
}

fn code_label(name: &str, marker_options_applied: bool) -> String {
    format!(
        "{name}{}",
        if marker_options_applied {
            " (marker options applied)"
        } else {
            ""
        }
    )
}

impl CheckSummary {
    pub(crate) fn build(asadoc_config: &AsadocConfig, evaluation: &Evaluation) -> Result<Self> {
        let unresolved = evaluation
            .assemblies
            .iter()
            .filter_map(unresolved_in_assembly)
            .collect();
        let shown_code_ids = shown_code_ids(evaluation);
        let unused_code = evaluation
            .unused
            .iter()
            .map(|&marked_index| describe_unused_code(evaluation, &shown_code_ids, marked_index))
            .filter_map(Result::transpose)
            .collect::<Result<_>>()
            .context("describing marked code no doc block matches")?;

        Ok(Self {
            docs_description: asadoc_config.docs.describe(),
            total_blocks: evaluation
                .assemblies
                .iter()
                .map(|assembly_eval| assembly_eval.blocks.len())
                .sum(),
            resolved_blocks: evaluation
                .blocks()
                .filter(|(_, block_eval)| block_eval.resolved())
                .count(),
            ignored_blocks: evaluation
                .blocks()
                .filter(|(_, block_eval)| block_eval.ignored_as.is_some())
                .count(),
            unresolved,
            unused_code,
            stale_ignored: evaluation
                .stale_ignored
                .iter()
                .map(|(reason, ignored_content)| {
                    (reason.clone(), ignored_content.lines().next().unwrap_or("").to_owned())
                })
                .collect(),
            problems: evaluation.scan.problems.clone(),
        })
    }
}

/// The blocks of an assembly still to resolve; None when there are none
fn unresolved_in_assembly(assembly_eval: &AssemblyEval) -> Option<UnresolvedInAssembly> {
    let unresolved_blocks: Vec<UnresolvedBlock> = assembly_eval
        .blocks
        .iter()
        .filter(|block_eval| !block_eval.done())
        .map(|block_eval| {
            let top_candidate = block_eval.candidates.first();
            UnresolvedBlock {
                reference: block_eval.block.reference.clone(),
                location: block_location(block_eval),
                closest: top_candidate.map(closest_from_candidate),
                fix_steps: top_candidate.and_then(fix_steps),
            }
        })
        .collect();
    (!unresolved_blocks.is_empty()).then(|| UnresolvedInAssembly {
        title: assembly_eval.assembly.title.clone(),
        path: assembly_eval.assembly.path.clone(),
        blocks: unresolved_blocks,
    })
}

/// The ids of the code shown as the closest of some block still to resolve
fn shown_code_ids(evaluation: &Evaluation) -> HashSet<String> {
    evaluation
        .blocks()
        .filter(|(_, block_eval)| !block_eval.done())
        .filter_map(|(_, block_eval)| block_eval.candidates.first())
        .map(|candidate| candidate.id.clone())
        .collect()
}

/// Marked code no block matches, by its index in `scan.marked`; None when it's
/// already shown as some block's closest code
fn describe_unused_code(
    evaluation: &Evaluation,
    shown_code_ids: &HashSet<String>,
    marked_index: usize,
) -> Result<Option<UnusedCode>> {
    let marked_code = evaluation
        .marked(marked_index)
        .context("looking up unmatched marked code")?;
    if shown_code_ids.contains(&marked_code.id) {
        return Ok(None);
    }
    let name = describe_code(marked_code);
    let closest_block = eval::closest_block(evaluation, marked_code)
        .with_context(|| format!("finding the doc block closest to {name}"))?
        .map(|(block_eval, doc_text, filled_code)| {
            (
                block_eval.block.reference.clone(),
                LineDiff::new(&doc_text, &filled_code),
            )
        });
    Ok(Some(UnusedCode { name, closest_block }))
}

/// What checking one given block or piece of marked code found
#[derive(Debug)]
pub(crate) enum CheckOutcome {
    NotFound {
        name: String,
    },
    Ignored {
        reference: String,
        reason: String,
    },
    Resolved {
        reference: String,
        matched_codes: Vec<String>,
        placeholder_values: Values,
    },
    /// No code matches the block; this is the closest
    Unmatched {
        reference: String,
        location: String,
        closest: Closest,
        fix_steps: Option<Vec<String>>,
        sides: Sides,
    },
    /// Nothing in the repo resembles the block
    NothingResembles {
        reference: String,
        location: String,
        block_content: String,
    },
    /// `--against` code that doesn't match the block
    Mismatch {
        reference: String,
        location: String,
        against_code: String,
        sides: Sides,
    },
    /// `--against` code whose doc options don't fit the block
    DocOptionsDontFit {
        reference: String,
        against_code: String,
    },
    CodeMatches {
        name: String,
        matched_blocks: Vec<String>,
    },
    CodeUnmatched {
        name: String,
        /// The closest block: reference, location, diff
        closest_block: Option<(String, String, Sides)>,
    },
}

impl CheckOutcome {
    pub(crate) const fn ok(&self) -> bool {
        matches!(
            self,
            Self::Ignored { .. } | Self::Resolved { .. } | Self::CodeMatches { .. }
        )
    }
}

/// Marked code a command-line argument names: `path` (all marked code in the
/// file), `path, section "name"` (as the report prints it) or `path#name`
fn find_marked_code<'a>(evaluation: &'a Evaluation, code_arg: &str) -> Vec<&'a MarkedCode> {
    evaluation
        .scan
        .marked
        .iter()
        .filter(|marked_code| {
            marked_code.id == code_arg || describe_code(marked_code) == code_arg || marked_code.file == code_arg
        })
        .collect()
}

/// Whether a block's match is with the given marked code
fn is_match_with(evaluation: &Evaluation, code_match: &CodeMatch, marked_code: &MarkedCode) -> bool {
    evaluation
        .marked(code_match.code)
        .is_ok_and(|matched_code| matched_code.id == marked_code.id)
}

fn matches_code(evaluation: &Evaluation, block_eval: &BlockEval, marked_code: &MarkedCode) -> bool {
    block_eval
        .matches
        .iter()
        .any(|code_match| is_match_with(evaluation, code_match, marked_code))
}

/// Checks blocks or marked code by name. With `against_arg`, blocks are
/// compared with that marked code instead of their closest.
pub(crate) fn check_outcomes(
    evaluation: &Evaluation,
    names: &[String],
    against_arg: Option<&str>,
) -> Result<Vec<CheckOutcome>> {
    let against_code = against_arg
        .map(|code_arg| find_against_code(evaluation, code_arg))
        .transpose()?;
    Ok(names
        .iter()
        .map(|name| name_outcomes(evaluation, name, against_code))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect())
}

/// The one piece of marked code `--against` names
fn find_against_code<'a>(evaluation: &'a Evaluation, code_arg: &str) -> Result<&'a MarkedCode> {
    match find_marked_code(evaluation, code_arg).as_slice() {
        [marked_code] => Ok(*marked_code),
        [] => bail!("no marked code named {code_arg}"),
        _ => bail!("{code_arg} has several marked sections; name one as `{code_arg}, section \"<name>\"`"),
    }
}

/// What checking the block, or all the marked code, a name refers to found
fn name_outcomes(evaluation: &Evaluation, name: &str, against_code: Option<&MarkedCode>) -> Result<Vec<CheckOutcome>> {
    if let Some((_, block_eval)) = evaluation
        .blocks()
        .find(|(_, block_eval)| block_eval.block.reference == name)
    {
        let outcome = match against_code {
            Some(marked_code) => block_outcome_against(evaluation, block_eval, marked_code)
                .with_context(|| format!("checking {name} against {}", describe_code(marked_code)))?,
            None => block_outcome(evaluation, block_eval).with_context(|| format!("checking {name}"))?,
        };
        return Ok(vec![outcome]);
    }
    let named_codes = find_marked_code(evaluation, name);
    if named_codes.is_empty() {
        return Ok(vec![CheckOutcome::NotFound { name: name.to_owned() }]);
    }
    named_codes
        .into_iter()
        .map(|marked_code| {
            code_outcome(evaluation, marked_code).with_context(|| format!("checking {}", describe_code(marked_code)))
        })
        .collect()
}

fn block_outcome_against(
    evaluation: &Evaluation,
    block_eval: &BlockEval,
    against_code: &MarkedCode,
) -> Result<CheckOutcome> {
    let reference = block_eval.block.reference.clone();
    if let Some(code_match) = block_eval
        .matches
        .iter()
        .find(|code_match| is_match_with(evaluation, code_match, against_code))
    {
        return Ok(CheckOutcome::Resolved {
            reference,
            matched_codes: vec![describe_code(against_code)],
            placeholder_values: code_match.values.clone(),
        });
    }
    let Some((doc_text, filled_code)) = eval::compare(against_code, &block_eval.block)
        .with_context(|| format!("comparing {} with {reference}", describe_code(against_code)))?
    else {
        return Ok(CheckOutcome::DocOptionsDontFit {
            reference,
            against_code: describe_code(against_code),
        });
    };
    Ok(CheckOutcome::Mismatch {
        reference,
        location: block_location(block_eval),
        against_code: describe_code(against_code),
        sides: Sides {
            doc_text,
            doc_label: doc_label(block_eval, !against_code.doc_options.is_empty()),
            code_text: filled_code,
            code_label: code_label(&describe_code(against_code), !against_code.options.is_empty()),
        },
    })
}

fn block_outcome(evaluation: &Evaluation, block_eval: &BlockEval) -> Result<CheckOutcome> {
    let reference = block_eval.block.reference.clone();
    if let Some(reason) = &block_eval.ignored_as {
        return Ok(CheckOutcome::Ignored {
            reference,
            reason: reason.clone(),
        });
    }
    if let Some(first_match) = block_eval.matches.first() {
        let matched_codes = block_eval
            .matches
            .iter()
            .map(|code_match| evaluation.marked(code_match.code).map(describe_code))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("describing the code {reference} matches"))?;
        return Ok(CheckOutcome::Resolved {
            reference,
            matched_codes,
            placeholder_values: first_match.values.clone(),
        });
    }
    let Some(top_candidate) = block_eval.candidates.first() else {
        return Ok(CheckOutcome::NothingResembles {
            reference,
            location: block_location(block_eval),
            block_content: block_eval.block.content.clone(),
        });
    };
    Ok(CheckOutcome::Unmatched {
        reference,
        location: block_location(block_eval),
        closest: closest_from_candidate(top_candidate),
        fix_steps: fix_steps(top_candidate),
        sides: Sides {
            doc_text: top_candidate.doc.clone(),
            doc_label: doc_label(block_eval, !top_candidate.doc_options.is_empty()),
            code_text: top_candidate.content.clone(),
            code_label: code_label(
                &code_name(&top_candidate.file, top_candidate.name.as_deref()),
                !top_candidate.options.is_empty(),
            ),
        },
    })
}

fn code_outcome(evaluation: &Evaluation, marked_code: &MarkedCode) -> Result<CheckOutcome> {
    let name = describe_code(marked_code);
    let matched_blocks: Vec<String> = evaluation
        .blocks()
        .filter(|(_, block_eval)| matches_code(evaluation, block_eval, marked_code))
        .map(|(_, block_eval)| block_eval.block.reference.clone())
        .collect();
    if !matched_blocks.is_empty() {
        return Ok(CheckOutcome::CodeMatches { name, matched_blocks });
    }
    let closest_block = eval::closest_block(evaluation, marked_code)
        .with_context(|| format!("finding the doc block closest to {name}"))?
        .map(|(block_eval, doc_text, filled_code)| {
            (
                block_eval.block.reference.clone(),
                block_location(block_eval),
                Sides {
                    doc_text,
                    doc_label: doc_label(block_eval, !marked_code.doc_options.is_empty()),
                    code_text: filled_code,
                    code_label: code_label(&name, !marked_code.options.is_empty()),
                },
            )
        });
    Ok(CheckOutcome::CodeUnmatched { name, closest_block })
}

/// What `asadoc fix` did
#[derive(Debug)]
pub(crate) enum FixOutcome {
    AlreadyDone,
    NoFix,
    Applied {
        changed_file: String,
        steps: Vec<String>,
        resolved: bool,
    },
}

impl FixOutcome {
    pub(crate) const fn ok(&self) -> bool {
        matches!(self, Self::AlreadyDone | Self::Applied { resolved: true, .. })
    }
}

/// Makes the change `asadoc check` lists under a block
pub(crate) fn fix(config: &AsadocConfig, reference: &str) -> Result<FixOutcome> {
    let evaluation = eval::evaluate(config, true).context("evaluating the doc blocks")?;
    let (_, block_eval) = evaluation
        .blocks()
        .find(|(_, block_eval)| block_eval.block.reference == reference)
        .with_context(|| format!("no doc block {reference} in the configured assemblies"))?;
    if block_eval.done() {
        return Ok(FixOutcome::AlreadyDone);
    }
    let Some((candidate, plan)) = block_eval
        .candidates
        .iter()
        .find_map(|candidate| candidate.plan.as_ref().map(|plan| (candidate, plan)))
    else {
        return Ok(FixOutcome::NoFix);
    };
    lightbulb::apply(&config.repo_root, &candidate.file, candidate.name.as_deref(), plan)
        .with_context(|| format!("changing {}", candidate.file))?;
    let reevaluation = eval::evaluate(config, false).context("checking the block again")?;
    Ok(FixOutcome::Applied {
        changed_file: candidate.file.clone(),
        steps: lightbulb::describe(plan, &candidate.file, candidate.name.as_deref()),
        resolved: reevaluation
            .blocks()
            .any(|(_, block_eval)| block_eval.block.reference == reference && block_eval.resolved()),
    })
}
