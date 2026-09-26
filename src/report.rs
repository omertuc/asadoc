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
    pub doc_lines: usize,
    /// Doc lines the code doesn't have
    pub differ: usize,
    /// Code lines the doc doesn't have
    pub extra: usize,
}

impl LineDiff {
    pub(crate) fn new(doc: &str, code: &str) -> Self {
        let (differ, extra) =
            TextDiff::from_lines(doc, code)
                .iter_all_changes()
                .fold((0, 0), |(differ, extra), change| match change.tag() {
                    ChangeTag::Delete => (differ + 1, extra),
                    ChangeTag::Insert => (differ, extra + 1),
                    ChangeTag::Equal => (differ, extra),
                });
        Self {
            doc_lines: doc.lines().count(),
            differ,
            extra,
        }
    }
}

/// A doc side and a code side, to show as a diff
#[derive(Debug)]
pub(crate) struct Sides {
    pub doc: String,
    pub doc_label: String,
    pub code: String,
    pub code_label: String,
}

/// The repo code most like a doc block
#[derive(Debug)]
pub(crate) enum Closest {
    Marked {
        name: String,
        diff: LineDiff,
    },
    UnmarkedFile {
        file: String,
        diff: LineDiff,
    },
    /// Lines of a file that, marked as a section, would match
    Lines {
        file: String,
        from: usize,
        to: usize,
    },
}

/// A doc block still to resolve
#[derive(Debug)]
pub(crate) struct UnresolvedBlock {
    pub reference: String,
    pub location: String,
    pub closest: Option<Closest>,
    /// The changes `asadoc fix` would make
    pub fix: Option<Vec<String>>,
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
pub(crate) struct Unused {
    pub name: String,
    /// The block most like it
    pub closest: Option<(String, LineDiff)>,
}

/// Everything `asadoc check` reports
#[derive(Debug)]
pub(crate) struct Summary {
    pub docs: String,
    pub total: usize,
    pub resolved: usize,
    pub ignored: usize,
    /// Blocks to resolve, by assembly
    pub unresolved: Vec<UnresolvedInAssembly>,
    /// Marked code no block matches that isn't any block's closest code
    pub unused: Vec<Unused>,
    /// (reason, first line) of ignored content no block has anymore
    pub stale_ignored: Vec<(String, String)>,
    pub problems: Vec<Problem>,
}

impl Summary {
    pub(crate) const fn open(&self) -> usize {
        self.total.saturating_sub(self.resolved + self.ignored)
    }
    pub(crate) fn fixable(&self) -> usize {
        self.unresolved
            .iter()
            .flat_map(|assembly| &assembly.blocks)
            .filter(|block| block.fix.is_some())
            .count()
    }
    pub(crate) const fn ok(&self) -> bool {
        self.open() == 0 && self.problems.is_empty()
    }
}

/// How marked code is named to people, and on the command line
pub(crate) fn code_name(file: &str, section: Option<&str>) -> String {
    match section {
        Some(section) => format!("{file}, section \"{section}\""),
        None => file.to_owned(),
    }
}

fn describe(code: &MarkedCode) -> String {
    code_name(&code.file, code.section.as_deref())
}

fn location(block_eval: &BlockEval) -> String {
    format!("modules/{}.adoc:{}", block_eval.block.module, block_eval.block.line)
}

fn closest(candidate: &CandidateInfo) -> Closest {
    match candidate.kind {
        "unmarked-file" => Closest::UnmarkedFile {
            file: candidate.file.clone(),
            diff: LineDiff::new(&candidate.doc, &candidate.content),
        },
        "lines" => {
            let (from, to) = candidate.lines.unwrap_or_default();
            Closest::Lines {
                file: candidate.file.clone(),
                from,
                to,
            }
        }
        _ => Closest::Marked {
            name: code_name(&candidate.file, candidate.name.as_deref()),
            diff: LineDiff::new(&candidate.doc, &candidate.content),
        },
    }
}

fn fix_steps(candidate: &CandidateInfo) -> Option<Vec<String>> {
    candidate
        .plan
        .as_ref()
        .map(|plan| lightbulb::describe(plan, &candidate.file, candidate.name.as_deref()))
}

fn doc_label(block_eval: &BlockEval, doc_options: bool) -> String {
    format!(
        "doc: {}{}",
        block_eval.block.reference,
        if doc_options { " (doc options applied)" } else { "" }
    )
}

fn code_label(name: &str, options: bool) -> String {
    format!("{name}{}", if options { " (marker options applied)" } else { "" })
}

impl Summary {
    pub(crate) fn build(asadoc_config: &AsadocConfig, evaluation: &Evaluation) -> Result<Self> {
        let unresolved = evaluation
            .assemblies
            .iter()
            .filter_map(unresolved_in_assembly)
            .collect();
        let shown_code = shown_code_ids(evaluation);
        let unused = evaluation
            .unused
            .iter()
            .map(|&index| unused_code(evaluation, &shown_code, index))
            .filter_map(Result::transpose)
            .collect::<Result<_>>()
            .context("describing marked code no doc block matches")?;

        Ok(Self {
            docs: asadoc_config.docs.describe(),
            total: evaluation
                .assemblies
                .iter()
                .map(|assembly_eval| assembly_eval.blocks.len())
                .sum(),
            resolved: evaluation
                .blocks()
                .filter(|(_, block_eval)| block_eval.resolved())
                .count(),
            ignored: evaluation
                .blocks()
                .filter(|(_, block_eval)| block_eval.ignored_as.is_some())
                .count(),
            unresolved,
            unused,
            stale_ignored: evaluation
                .stale_ignored
                .iter()
                .map(|(reason, content)| (reason.clone(), content.lines().next().unwrap_or("").to_owned()))
                .collect(),
            problems: evaluation.scan.problems.clone(),
        })
    }
}

/// The blocks of an assembly still to resolve; None when there are none
fn unresolved_in_assembly(assembly_eval: &AssemblyEval) -> Option<UnresolvedInAssembly> {
    let blocks: Vec<UnresolvedBlock> = assembly_eval
        .blocks
        .iter()
        .filter(|block_eval| !block_eval.done())
        .map(|block_eval| {
            let top = block_eval.candidates.first();
            UnresolvedBlock {
                reference: block_eval.block.reference.clone(),
                location: location(block_eval),
                closest: top.map(closest),
                fix: top.and_then(fix_steps),
            }
        })
        .collect();
    (!blocks.is_empty()).then(|| UnresolvedInAssembly {
        title: assembly_eval.assembly.title.clone(),
        path: assembly_eval.assembly.path.clone(),
        blocks,
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
fn unused_code(evaluation: &Evaluation, shown_code: &HashSet<String>, index: usize) -> Result<Option<Unused>> {
    let code = evaluation.marked(index).context("looking up unmatched marked code")?;
    if shown_code.contains(&code.id) {
        return Ok(None);
    }
    let name = describe(code);
    let closest = eval::closest_block(evaluation, code)
        .with_context(|| format!("finding the doc block closest to {name}"))?
        .map(|(block_eval, doc, filled)| (block_eval.block.reference.clone(), LineDiff::new(&doc, &filled)));
    Ok(Some(Unused { name, closest }))
}

/// What checking one given block or piece of marked code found
#[derive(Debug)]
pub(crate) enum Outcome {
    NotFound {
        name: String,
    },
    Ignored {
        reference: String,
        reason: String,
    },
    Resolved {
        reference: String,
        codes: Vec<String>,
        values: Values,
    },
    /// No code matches the block; this is the closest
    Unmatched {
        reference: String,
        location: String,
        closest: Closest,
        fix: Option<Vec<String>>,
        sides: Sides,
    },
    /// Nothing in the repo resembles the block
    Alone {
        reference: String,
        location: String,
        content: String,
    },
    /// `--against` code that doesn't match the block
    Mismatch {
        reference: String,
        location: String,
        code: String,
        sides: Sides,
    },
    /// `--against` code whose doc options don't fit the block
    DocOptionsDontFit {
        reference: String,
        code: String,
    },
    CodeMatches {
        name: String,
        blocks: Vec<String>,
    },
    CodeUnmatched {
        name: String,
        /// The closest block: reference, location, diff
        closest: Option<(String, String, Sides)>,
    },
}

impl Outcome {
    pub(crate) const fn ok(&self) -> bool {
        matches!(
            self,
            Self::Ignored { .. } | Self::Resolved { .. } | Self::CodeMatches { .. }
        )
    }
}

/// Marked code a command-line argument names: `path` (all marked code in the
/// file), `path, section "name"` (as the report prints it) or `path#name`
fn find_code<'a>(evaluation: &'a Evaluation, arg: &str) -> Vec<&'a MarkedCode> {
    evaluation
        .scan
        .marked
        .iter()
        .filter(|code| code.id == arg || describe(code) == arg || code.file == arg)
        .collect()
}

/// Whether a block's match is with the given marked code
fn is_match_with(evaluation: &Evaluation, code_match: &CodeMatch, code: &MarkedCode) -> bool {
    evaluation
        .marked(code_match.code)
        .is_ok_and(|matched| matched.id == code.id)
}

fn matches_code(evaluation: &Evaluation, block_eval: &BlockEval, code: &MarkedCode) -> bool {
    block_eval
        .matches
        .iter()
        .any(|code_match| is_match_with(evaluation, code_match, code))
}

/// Checks blocks or marked code by name. With `against`, blocks are compared
/// with that marked code instead of their closest.
pub(crate) fn outcomes(evaluation: &Evaluation, names: &[String], against: Option<&str>) -> Result<Vec<Outcome>> {
    let against = against.map(|arg| against_code(evaluation, arg)).transpose()?;
    Ok(names
        .iter()
        .map(|name| name_outcomes(evaluation, name, against))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect())
}

/// The one piece of marked code `--against` names
fn against_code<'a>(evaluation: &'a Evaluation, arg: &str) -> Result<&'a MarkedCode> {
    match find_code(evaluation, arg).as_slice() {
        [code] => Ok(*code),
        [] => bail!("no marked code named {arg}"),
        _ => bail!("{arg} has several marked sections; name one as `{arg}, section \"<name>\"`"),
    }
}

/// What checking the block, or all the marked code, a name refers to found
fn name_outcomes(evaluation: &Evaluation, name: &str, against: Option<&MarkedCode>) -> Result<Vec<Outcome>> {
    if let Some((_, block_eval)) = evaluation
        .blocks()
        .find(|(_, block_eval)| block_eval.block.reference == name)
    {
        let outcome = match against {
            Some(code) => block_against(evaluation, block_eval, code)
                .with_context(|| format!("checking {name} against {}", describe(code)))?,
            None => block(evaluation, block_eval).with_context(|| format!("checking {name}"))?,
        };
        return Ok(vec![outcome]);
    }
    let codes = find_code(evaluation, name);
    if codes.is_empty() {
        return Ok(vec![Outcome::NotFound { name: name.to_owned() }]);
    }
    codes
        .into_iter()
        .map(|code| code_outcome(evaluation, code).with_context(|| format!("checking {}", describe(code))))
        .collect()
}

fn block_against(evaluation: &Evaluation, block_eval: &BlockEval, code: &MarkedCode) -> Result<Outcome> {
    let reference = block_eval.block.reference.clone();
    if let Some(code_match) = block_eval
        .matches
        .iter()
        .find(|code_match| is_match_with(evaluation, code_match, code))
    {
        return Ok(Outcome::Resolved {
            reference,
            codes: vec![describe(code)],
            values: code_match.values.clone(),
        });
    }
    let Some((doc, filled)) = eval::compare(code, &block_eval.block)
        .with_context(|| format!("comparing {} with {reference}", describe(code)))?
    else {
        return Ok(Outcome::DocOptionsDontFit {
            reference,
            code: describe(code),
        });
    };
    Ok(Outcome::Mismatch {
        reference,
        location: location(block_eval),
        code: describe(code),
        sides: Sides {
            doc,
            doc_label: doc_label(block_eval, !code.doc_options.is_empty()),
            code: filled,
            code_label: code_label(&describe(code), !code.options.is_empty()),
        },
    })
}

fn block(evaluation: &Evaluation, block_eval: &BlockEval) -> Result<Outcome> {
    let reference = block_eval.block.reference.clone();
    if let Some(reason) = &block_eval.ignored_as {
        return Ok(Outcome::Ignored {
            reference,
            reason: reason.clone(),
        });
    }
    if let Some(first) = block_eval.matches.first() {
        let codes = block_eval
            .matches
            .iter()
            .map(|code_match| evaluation.marked(code_match.code).map(describe))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("describing the code {reference} matches"))?;
        return Ok(Outcome::Resolved {
            reference,
            codes,
            values: first.values.clone(),
        });
    }
    let Some(top) = block_eval.candidates.first() else {
        return Ok(Outcome::Alone {
            reference,
            location: location(block_eval),
            content: block_eval.block.content.clone(),
        });
    };
    Ok(Outcome::Unmatched {
        reference,
        location: location(block_eval),
        closest: closest(top),
        fix: fix_steps(top),
        sides: Sides {
            doc: top.doc.clone(),
            doc_label: doc_label(block_eval, !top.doc_options.is_empty()),
            code: top.content.clone(),
            code_label: code_label(&code_name(&top.file, top.name.as_deref()), !top.options.is_empty()),
        },
    })
}

fn code_outcome(evaluation: &Evaluation, code: &MarkedCode) -> Result<Outcome> {
    let name = describe(code);
    let blocks: Vec<String> = evaluation
        .blocks()
        .filter(|(_, block_eval)| matches_code(evaluation, block_eval, code))
        .map(|(_, block_eval)| block_eval.block.reference.clone())
        .collect();
    if !blocks.is_empty() {
        return Ok(Outcome::CodeMatches { name, blocks });
    }
    let closest = eval::closest_block(evaluation, code)
        .with_context(|| format!("finding the doc block closest to {name}"))?
        .map(|(block_eval, doc, filled)| {
            (
                block_eval.block.reference.clone(),
                location(block_eval),
                Sides {
                    doc,
                    doc_label: doc_label(block_eval, !code.doc_options.is_empty()),
                    code: filled,
                    code_label: code_label(&name, !code.options.is_empty()),
                },
            )
        });
    Ok(Outcome::CodeUnmatched { name, closest })
}

/// What `asadoc fix` did
#[derive(Debug)]
pub(crate) enum FixOutcome {
    AlreadyDone,
    NoFix,
    Applied {
        file: String,
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
    let after = eval::evaluate(config, false).context("checking the block again")?;
    Ok(FixOutcome::Applied {
        file: candidate.file.clone(),
        steps: lightbulb::describe(plan, &candidate.file, candidate.name.as_deref()),
        resolved: after
            .blocks()
            .any(|(_, block_eval)| block_eval.block.reference == reference && block_eval.resolved()),
    })
}
