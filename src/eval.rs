//! Evaluation: every doc block against all marked code and the ignore directory.
//!
//! A block is resolved when some marked code (its options applied, its doc
//! options applied to the block, its placeholders free) matches it; ignored
//! when its content is in the ignore directory; and otherwise still to resolve,
//! with the repo code most like it as candidates.

use crate::config::AsadocConfig;
use crate::docs::{self, Assembly, Block};
use crate::ignored::Ignored;
use crate::lightbulb::{self, Candidate, Fix, Plan};
use crate::markers::{self, MarkerOption};
use crate::matching::Values;
use crate::repo::{self, MarkedCode, RepoFile, Scan};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::iter;

const MAX_CANDIDATES: usize = 5;
const MIN_SIMILARITY: f64 = 0.3;

pub(crate) struct CodeMatch {
    /// Index into `Evaluation::scan.marked`
    pub code: usize,
    pub values: Values,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CandidateInfo {
    pub id: String,
    /// section, file (marked), lines (would become a section), unmarked-file
    pub kind: &'static str,
    pub file: String,
    pub name: Option<String>,
    pub lines: Option<(usize, usize)>,
    pub marker_lines: Vec<usize>,
    pub options: Vec<MarkerOption>,
    pub doc_options: Vec<MarkerOption>,
    pub placeholders: Vec<String>,
    pub plan: Option<Plan>,
    pub similarity: f64,
    /// What the block is compared with: the code (placeholders filled in where
    /// possible), the would-be section, or the whole unmarked file
    pub content: String,
    /// The doc side it's compared with (doc options applied)
    pub doc: String,
    pub file_text: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct Formerly {
    pub reason: String,
    pub content: String,
    pub similarity: f64,
}

pub(crate) struct BlockEval {
    pub block: Block,
    pub matches: Vec<CodeMatch>,
    pub ignored_as: Option<String>,
    pub candidates: Vec<CandidateInfo>,
    /// For an unresolved block: a stale ignored entry it probably used to be
    pub formerly: Vec<Formerly>,
}

impl BlockEval {
    pub(crate) const fn resolved(&self) -> bool {
        !self.matches.is_empty()
    }
    pub(crate) const fn done(&self) -> bool {
        self.resolved() || self.ignored_as.is_some()
    }
}

pub(crate) struct AssemblyEval {
    pub assembly: Assembly,
    pub blocks: Vec<BlockEval>,
}

pub(crate) struct Evaluation {
    pub assemblies: Vec<AssemblyEval>,
    pub scan: Scan,
    /// Ignored entries no doc block has anymore: (reason, content)
    pub stale_ignored: Vec<(String, String)>,
    /// Marked code no doc block matches (indexes into `scan.marked`)
    pub unused: Vec<usize>,
}

impl Evaluation {
    pub(crate) fn blocks(&self) -> impl Iterator<Item = (&AssemblyEval, &BlockEval)> {
        self.assemblies.iter().flat_map(|assembly_eval| {
            assembly_eval
                .blocks
                .iter()
                .map(move |block_eval| (assembly_eval, block_eval))
        })
    }
    pub(crate) fn find(&self, assembly: &str, reference: &str) -> Option<&BlockEval> {
        self.blocks()
            .find(|(assembly_eval, block_eval)| {
                assembly_eval.assembly.id == assembly && block_eval.block.reference == reference
            })
            .map(|(_, block_eval)| block_eval)
    }
    /// Marked code, by its index in `scan.marked`
    pub(crate) fn marked(&self, code: usize) -> Result<&MarkedCode> {
        self.scan
            .marked
            .get(code)
            .with_context(|| format!("no marked code at index {code}"))
    }
}

// ---------------------------------------------------------------------------
// Similarity
// ---------------------------------------------------------------------------

fn tokens(text: &str) -> Result<Vec<String>> {
    Ok(regex!(r"[a-z0-9_.$-]+")?
        .find_iter(&text.to_lowercase())
        .map(|token| token.as_str().to_owned())
        .collect())
}

#[expect(
    clippy::cast_precision_loss,
    reason = "token and line counts are far below 2^52, where f64 stops being exact"
)]
const fn count_to_f64(count: usize) -> f64 {
    count as f64
}

/// Takes one `token` from `remaining`; false when none is left
fn take_token(remaining: &mut HashMap<&str, usize>, token: &str) -> bool {
    match remaining.get_mut(token) {
        Some(count) if *count > 0 => {
            *count -= 1;
            true
        }
        _ => false,
    }
}

fn dice(first: &[String], second: &[String]) -> f64 {
    if first.is_empty() || second.is_empty() {
        return 0.0;
    }
    let mut remaining = first.iter().fold(HashMap::<&str, usize>::new(), |mut counts, token| {
        *counts.entry(token).or_default() += 1;
        counts
    });
    let common = second.iter().filter(|token| take_token(&mut remaining, token)).count();
    2.0 * count_to_f64(common) / count_to_f64(first.len() + second.len())
}

/// The lines of `text`, trimmed, blank ones skipped
fn significant_lines(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).collect()
}

/// The next row of the LCS table: `previous[j]` is the LCS of the lines so far
/// and the first j of `other_lines`, and the result is the same with `line` added
fn next_lcs_row(previous: &[usize], line: &str, other_lines: &[&str]) -> Vec<usize> {
    let cells = other_lines
        .iter()
        .zip(previous.iter().zip(previous.iter().skip(1)))
        .scan(0, |left, (other_line, (diagonal, up))| {
            *left = if line == *other_line {
                diagonal + 1
            } else {
                (*up).max(*left)
            };
            Some(*left)
        });
    iter::once(0).chain(cells).collect()
}

/// Share of lines (trimmed, blank ones skipped) in their longest common subsequence
fn line_ratio(first: &str, second: &str) -> f64 {
    let first_lines = significant_lines(first);
    let second_lines = significant_lines(second);
    if first_lines.is_empty() || second_lines.is_empty() {
        return 0.0;
    }
    let last_row = first_lines
        .iter()
        .fold(vec![0usize; second_lines.len() + 1], |previous, line| {
            next_lcs_row(&previous, line, &second_lines)
        });
    let common = last_row.last().copied().unwrap_or(0);
    2.0 * count_to_f64(common) / count_to_f64(first_lines.len() + second_lines.len())
}

/// How alike two texts are, given their tokens' dice score: that alone when
/// it's too low, otherwise blended with how their lines line up
fn blended_similarity(token_score: f64, first: &str, second: &str) -> f64 {
    if token_score < MIN_SIMILARITY {
        token_score
    } else {
        0.5f64.mul_add(line_ratio(first, second), 0.5 * token_score)
    }
}

fn similarity(first: &str, second: &str) -> Result<f64> {
    let token_score = dice(&tokens(first)?, &tokens(second)?);
    Ok(blended_similarity(token_score, first, second))
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Some marked code next to a doc block, for a diff: the block with the code's
/// doc options applied, and the code with its placeholders filled in from it.
/// None when the doc options don't fit the block.
pub(crate) fn compare(code: &MarkedCode, block: &Block) -> Result<Option<(String, String)>> {
    let Some(doc) = doc_side(block, &code.doc_options) else {
        return Ok(None);
    };
    let filled = code
        .matcher
        .fill(&doc)
        .with_context(|| format!("filling in the placeholders of {}", code.id))?;
    Ok(Some((doc, filled)))
}

/// The doc block most like some marked code, with `compare`'s two sides
pub(crate) fn closest_block<'a>(
    evaluation: &'a Evaluation,
    code: &MarkedCode,
) -> Result<Option<(&'a BlockEval, String, String)>> {
    let best = evaluation.blocks().try_fold(
        None::<(f64, &BlockEval, String, String)>,
        |best, (_, block_eval)| -> Result<_> {
            let Some((doc, filled)) = compare(code, &block_eval.block)? else {
                return Ok(best);
            };
            let score = similarity(&doc, &filled)
                .with_context(|| format!("comparing {} with {}", code.id, block_eval.block.reference))?;
            let is_better = score >= MIN_SIMILARITY && best.as_ref().is_none_or(|(best_score, ..)| score > *best_score);
            Ok(if is_better {
                Some((score, block_eval, doc, filled))
            } else {
                best
            })
        },
    )?;
    Ok(best.map(|(_, block_eval, doc, filled)| (block_eval, doc, filled)))
}

/// The block with doc options applied; None when they don't fit it (so no code with them can match)
fn doc_side(block: &Block, doc_options: &[MarkerOption]) -> Option<String> {
    markers::apply_options(&block.content, doc_options).ok()
}

pub(crate) fn evaluate(config: &AsadocConfig, with_candidates: bool) -> Result<Evaluation> {
    let scan = repo::scan(config).context("scanning the repo for marked code")?;
    let ignored = Ignored::load(&config.ignore_dir).context("loading the ignore directory")?;

    let mut assemblies = read_assemblies(config)?
        .into_iter()
        .map(|assembly| {
            let id = assembly.id.clone();
            evaluate_assembly(config, assembly, &scan, &ignored)
                .with_context(|| format!("evaluating the assembly {id}"))
        })
        .collect::<Result<Vec<_>>>()?;

    let stale_ignored = stale_ignored(&assemblies, &ignored);
    let unused = unused_code(&assemblies, &scan);

    if with_candidates {
        add_candidates(&mut assemblies, &scan, &stale_ignored).context("finding code like the blocks to resolve")?;
    }

    Ok(Evaluation {
        assemblies,
        scan,
        stale_ignored,
        unused,
    })
}

/// The configured assemblies, with every file evaluating them reads
/// prefetched: in two fetches when the docs come from git
fn read_assemblies(config: &AsadocConfig) -> Result<Vec<Assembly>> {
    config
        .docs
        .prefetch(&config.assemblies)
        .context("prefetching the assemblies")?;
    let assemblies = config
        .assemblies
        .iter()
        .map(|path| docs::read_assembly(&config.docs, path).with_context(|| format!("reading the assembly {path}")))
        .collect::<Result<Vec<_>>>()?;
    let module_paths = assemblies
        .iter()
        .flat_map(|assembly| assembly.modules.iter().map(|module| docs::module_path(module)))
        .collect::<Vec<_>>();
    config.docs.prefetch(&module_paths).context("prefetching the modules")?;
    Ok(assemblies)
}

fn evaluate_assembly(
    config: &AsadocConfig,
    assembly: Assembly,
    scan: &Scan,
    ignored: &Ignored,
) -> Result<AssemblyEval> {
    let blocks = assembly
        .modules
        .iter()
        .map(|module| evaluate_module(config, module, scan, ignored))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(AssemblyEval { assembly, blocks })
}

fn evaluate_module(config: &AsadocConfig, module: &str, scan: &Scan, ignored: &Ignored) -> Result<Vec<BlockEval>> {
    docs::extract_blocks(&config.docs, module)
        .with_context(|| format!("reading the module {module}"))?
        .into_iter()
        .map(|block| evaluate_block(block, scan, ignored))
        .collect()
}

fn evaluate_block(block: Block, scan: &Scan, ignored: &Ignored) -> Result<BlockEval> {
    let matches = match_block(&block, &scan.marked)?;
    let ignored_as = if matches.is_empty() {
        ignored.reason_of(&block.content).map(str::to_owned)
    } else {
        None
    };
    Ok(BlockEval {
        block,
        matches,
        ignored_as,
        candidates: vec![],
        formerly: vec![],
    })
}

/// All the marked code that matches the block
fn match_block(block: &Block, marked: &[MarkedCode]) -> Result<Vec<CodeMatch>> {
    // The block as each set of doc options makes it
    let mut sides: BTreeMap<String, Option<String>> = BTreeMap::new();
    marked
        .iter()
        .enumerate()
        .filter_map(|(index, code)| {
            let doc = sides
                .entry(format!("{:?}", code.doc_options))
                .or_insert_with(|| doc_side(block, &code.doc_options))
                .as_deref()?;
            code.matcher
                .matches(doc)
                .with_context(|| format!("matching {} against {}", code.id, block.reference))
                .transpose()
                .map(|values| values.map(|values| CodeMatch { code: index, values }))
        })
        .collect()
}

/// Ignored entries no doc block has anymore: (reason, content)
fn stale_ignored(assemblies: &[AssemblyEval], ignored: &Ignored) -> Vec<(String, String)> {
    let used: HashSet<&str> = assemblies
        .iter()
        .flat_map(|assembly_eval| &assembly_eval.blocks)
        .filter(|block_eval| block_eval.ignored_as.is_some())
        .map(|block_eval| block_eval.block.content.as_str())
        .collect();
    ignored
        .entries
        .iter()
        .filter(|entry| !used.contains(entry.content.as_str()))
        .map(|entry| (entry.reason.clone(), entry.content.clone()))
        .collect()
}

/// Marked code no doc block matches (indexes into `scan.marked`)
fn unused_code(assemblies: &[AssemblyEval], scan: &Scan) -> Vec<usize> {
    let matched: HashSet<usize> = assemblies
        .iter()
        .flat_map(|assembly_eval| &assembly_eval.blocks)
        .flat_map(|block_eval| &block_eval.matches)
        .map(|code_match| code_match.code)
        .collect();
    (0..scan.marked.len())
        .filter(|index| !matched.contains(index))
        .collect()
}

/// For each block still to resolve: the code most like it, and the stale
/// ignored content it probably used to be
fn add_candidates(assemblies: &mut [AssemblyEval], scan: &Scan, stale_ignored: &[(String, String)]) -> Result<()> {
    let pool = candidate_pool(scan).context("tokenizing the repo code")?;
    assemblies
        .iter_mut()
        .flat_map(|assembly_eval| &mut assembly_eval.blocks)
        .filter(|block_eval| !block_eval.done())
        .try_for_each(|block_eval| {
            block_eval.candidates = candidates_for(&block_eval.block, &pool)
                .with_context(|| format!("finding code resembling {}", block_eval.block.reference))?;
            block_eval.formerly = formerly(&block_eval.block, stale_ignored)
                .with_context(|| format!("comparing {} with ignored content", block_eval.block.reference))?
                .into_iter()
                .collect();
            Ok(())
        })
}

/// The stale ignored content most like the block (the last of equals), when
/// any is like it enough
fn formerly(block: &Block, stale_ignored: &[(String, String)]) -> Result<Option<Formerly>> {
    stale_ignored
        .iter()
        .try_fold(None::<Formerly>, |best, (reason, content)| {
            let score = similarity(&block.content, content)?;
            let is_better = score >= 0.5 && best.as_ref().is_none_or(|best| score >= best.similarity);
            Ok(if is_better {
                Some(Formerly {
                    reason: reason.clone(),
                    content: content.clone(),
                    similarity: score,
                })
            } else {
                best
            })
        })
}

struct PoolEntry<'a> {
    id: String,
    candidate: Candidate<'a>,
    tokens: Vec<String>,
}

/// All marked code (in a scanned file), then every unmarked file
fn candidate_pool(scan: &Scan) -> Result<Vec<PoolEntry<'_>>> {
    let files: HashMap<&str, &RepoFile> = scan.files.iter().map(|file| (file.file.as_str(), file)).collect();
    let marked = scan
        .marked
        .iter()
        .filter_map(|code| Some(marked_pool_entry(code, files.get(code.file.as_str())?)));
    let unmarked = scan
        .files
        .iter()
        .filter(|file| file.markers.file.is_none())
        .map(unmarked_pool_entry);
    marked.chain(unmarked).collect()
}

fn marked_pool_entry<'a>(code: &'a MarkedCode, file: &'a RepoFile) -> Result<PoolEntry<'a>> {
    Ok(PoolEntry {
        id: code.id.clone(),
        tokens: tokens(&code.content).with_context(|| format!("tokenizing {}", code.id))?,
        candidate: Candidate {
            file: &code.file,
            section: code.section.as_deref(),
            marked: Some(code),
            text: &file.text,
            markers: &file.markers,
        },
    })
}

fn unmarked_pool_entry(file: &RepoFile) -> Result<PoolEntry<'_>> {
    Ok(PoolEntry {
        id: file.file.clone(),
        tokens: tokens(&file.text).with_context(|| format!("tokenizing {}", file.file))?,
        candidate: Candidate {
            file: &file.file,
            section: None,
            marked: None,
            text: &file.text,
            markers: &file.markers,
        },
    })
}

fn candidates_for(block: &Block, pool: &[PoolEntry<'_>]) -> Result<Vec<CandidateInfo>> {
    let unprompted = unprompted_content(block);
    let unprompted_tokens = tokens(&unprompted).context("tokenizing the block")?;
    let mut scored = pool
        .iter()
        .filter_map(|entry| score_candidate(block, entry, &unprompted, &unprompted_tokens).transpose())
        .collect::<Result<Vec<_>>>()?;
    scored.sort_by(|first, second| {
        second
            .plan
            .is_some()
            .cmp(&first.plan.is_some())
            .then(second.similarity.total_cmp(&first.similarity))
    });
    scored.truncate(MAX_CANDIDATES);
    Ok(scored)
}

/// The block's content without its shell prompts, when it has any
fn unprompted_content(block: &Block) -> String {
    if block.content.lines().any(|line| line.starts_with("$ ")) {
        markers::apply_options(&block.content, &[lightbulb::prompt_option()]).unwrap_or_else(|_| block.content.clone())
    } else {
        block.content.clone()
    }
}

/// A pool entry as a candidate for the block; None when it's too unlike it
fn score_candidate(
    block: &Block,
    entry: &PoolEntry<'_>,
    unprompted: &str,
    unprompted_tokens: &[String],
) -> Result<Option<CandidateInfo>> {
    let plan =
        lightbulb::plan_for(block, &entry.candidate).with_context(|| format!("looking for a fix in {}", entry.id))?;
    let code = entry.candidate.marked;
    let doc_options = candidate_doc_options(code, plan.as_ref());
    let doc = doc_side(block, &doc_options).unwrap_or_else(|| block.content.clone());
    let section_to_mark = plan.as_ref().and_then(section_to_mark);
    let content = candidate_content(&entry.candidate, section_to_mark, &doc)?;
    let similarity = if plan.is_some() {
        1.0
    } else {
        let score = blended_similarity(dice(unprompted_tokens, &entry.tokens), unprompted, &content);
        if score < MIN_SIMILARITY {
            return Ok(None);
        }
        score
    };
    Ok(Some(CandidateInfo {
        id: entry.id.clone(),
        kind: candidate_kind(section_to_mark, code),
        file: entry.candidate.file.to_owned(),
        name: entry.candidate.section.map(str::to_owned),
        lines: section_to_mark
            .map(|(line, count)| (line, line + count - 1))
            .or_else(|| code.and_then(|code| code.lines)),
        marker_lines: code.map(|code| code.marker_lines.clone()).unwrap_or_default(),
        options: code.map(|code| code.options.clone()).unwrap_or_default(),
        doc_options,
        placeholders: code.map(|code| code.placeholders.clone()).unwrap_or_default(),
        plan,
        similarity,
        content,
        doc,
        file_text: entry.candidate.text.to_owned(),
    }))
}

/// The code's own doc options, or else the ones the plan would add
fn candidate_doc_options(code: Option<&MarkedCode>, plan: Option<&Plan>) -> Vec<MarkerOption> {
    match code {
        Some(code) if !code.doc_options.is_empty() => code.doc_options.clone(),
        _ => plan.map(|plan| plan.doc_options.clone()).unwrap_or_default(),
    }
}

/// The lines the plan would mark as a section: (first line, count)
fn section_to_mark(plan: &Plan) -> Option<(usize, usize)> {
    plan.fixes.iter().find_map(|fix| match fix {
        Fix::MarkSection { line, lines, .. } => Some((*line, *lines)),
        _ => None,
    })
}

/// What the block is compared with: the would-be section, the code with its
/// placeholders filled in, or the whole unmarked file
fn candidate_content(candidate: &Candidate<'_>, section_to_mark: Option<(usize, usize)>, doc: &str) -> Result<String> {
    Ok(match (section_to_mark, candidate.marked) {
        (Some((line, count)), _) => {
            candidate
                .text
                .split('\n')
                .skip(line - 1)
                .take(count)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        }
        (None, Some(code)) => code
            .matcher
            .fill(doc)
            .with_context(|| format!("filling in the placeholders of {}", code.id))?,
        (None, None) => candidate.text.to_owned(),
    })
}

const fn candidate_kind(section_to_mark: Option<(usize, usize)>, code: Option<&MarkedCode>) -> &'static str {
    match (section_to_mark, code) {
        (Some(_), _) => "lines",
        (None, Some(code)) if code.section.is_some() => "section",
        (None, Some(_)) => "file",
        (None, None) => "unmarked-file",
    }
}
