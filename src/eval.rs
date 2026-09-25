//! Evaluation: every doc block against all marked code and the ignored list.
//!
//! A block is resolved when some marked code (its options applied, its doc
//! options applied to the block, its placeholders free) matches it; ignored
//! when its content is in the ignored list; and otherwise still to resolve,
//! with the repo code most like it as candidates.

use crate::config::Config;
use crate::docs::{self, Assembly, Block};
use crate::ignored::Ignored;
use crate::lightbulb::{self, Candidate, Fix, Plan};
use crate::markers::{self, MarkerOption};
use crate::matching::Values;
use crate::repo::{self, MarkedCode, Scan};
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

const MAX_CANDIDATES: usize = 5;
const MIN_SIMILARITY: f64 = 0.3;

pub struct CodeMatch {
    /// Index into `Evaluation::scan.marked`
    pub code: usize,
    pub values: Values,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CandidateInfo {
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
pub struct Formerly {
    pub reason: String,
    pub content: String,
    pub similarity: f64,
}

pub struct BlockEval {
    pub block: Block,
    pub matches: Vec<CodeMatch>,
    pub ignored_as: Option<String>,
    pub candidates: Vec<CandidateInfo>,
    /// For an unresolved block: a stale ignored entry it probably used to be
    pub formerly: Vec<Formerly>,
}

impl BlockEval {
    pub fn resolved(&self) -> bool {
        !self.matches.is_empty()
    }
    pub fn done(&self) -> bool {
        self.resolved() || self.ignored_as.is_some()
    }
}

pub struct AssemblyEval {
    pub assembly: Assembly,
    pub blocks: Vec<BlockEval>,
}

pub struct Evaluation {
    pub assemblies: Vec<AssemblyEval>,
    pub scan: Scan,
    /// Ignored entries no doc block has anymore: (reason, content)
    pub stale_ignored: Vec<(String, String)>,
    /// Marked code no doc block matches (indexes into `scan.marked`)
    pub unused: Vec<usize>,
}

impl Evaluation {
    pub fn blocks(&self) -> impl Iterator<Item = (&AssemblyEval, &BlockEval)> {
        self.assemblies.iter().flat_map(|a| a.blocks.iter().map(move |b| (a, b)))
    }
    pub fn find(&self, assembly: &str, reference: &str) -> Option<&BlockEval> {
        self.blocks().find(|(a, b)| a.assembly.id == assembly && b.block.reference == reference).map(|(_, b)| b)
    }
}

// ---------------------------------------------------------------------------
// Similarity
// ---------------------------------------------------------------------------

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9_.$-]+").unwrap());

fn tokens(s: &str) -> Vec<String> {
    TOKEN.find_iter(&s.to_lowercase()).map(|m| m.as_str().to_string()).collect()
}

fn dice(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in a {
        *counts.entry(t).or_default() += 1;
    }
    let mut common = 0;
    for t in b {
        if let Some(c) = counts.get_mut(t.as_str()).filter(|c| **c > 0) {
            common += 1;
            *c -= 1;
        }
    }
    2.0 * common as f64 / (a.len() + b.len()) as f64
}

/// Share of lines (trimmed, blank ones skipped) in their longest common subsequence
fn line_ratio(a: &str, b: &str) -> f64 {
    let la: Vec<&str> = a.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let lb: Vec<&str> = b.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if la.is_empty() || lb.is_empty() {
        return 0.0;
    }
    let mut prev = vec![0usize; lb.len() + 1];
    for x in &la {
        let mut cur = vec![0usize; lb.len() + 1];
        for (j, y) in lb.iter().enumerate() {
            cur[j + 1] = if x == y { prev[j] + 1 } else { prev[j + 1].max(cur[j]) };
        }
        prev = cur;
    }
    2.0 * prev[lb.len()] as f64 / (la.len() + lb.len()) as f64
}

fn similarity(a: &str, b: &str) -> f64 {
    let d = dice(&tokens(a), &tokens(b));
    if d < MIN_SIMILARITY { d } else { 0.5 * d + 0.5 * line_ratio(a, b) }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

fn doc_side(block: &Block, doc_options: &[MarkerOption]) -> Option<String> {
    markers::apply_options(&block.content, doc_options).ok()
}

pub fn evaluate(config: &Config, with_candidates: bool) -> anyhow::Result<Evaluation> {
    let scan = repo::scan(config);
    let ignored = Ignored::load(&config.ignored_file)?;
    let mut matched = vec![false; scan.marked.len()];
    let mut used_ignored: std::collections::HashSet<String> = Default::default();

    let mut assemblies = Vec::new();
    for path in &config.assemblies {
        let assembly = docs::read_assembly(&config.docs_root, path);
        let mut blocks = Vec::new();
        for module in &assembly.modules {
            for block in docs::extract_blocks(&config.docs_root, module) {
                // The block as each set of doc options makes it
                let mut sides: BTreeMap<String, Option<String>> = BTreeMap::new();
                let mut matches = Vec::new();
                for (i, code) in scan.marked.iter().enumerate() {
                    let key = format!("{:?}", code.doc_options);
                    let doc = sides.entry(key).or_insert_with(|| doc_side(&block, &code.doc_options));
                    if let Some(values) = doc.as_deref().and_then(|d| code.matcher.matches(d)) {
                        matched[i] = true;
                        matches.push(CodeMatch { code: i, values });
                    }
                }
                let ignored_as = if matches.is_empty() { ignored.reason_of(&block.content).map(str::to_string) } else { None };
                if ignored_as.is_some() {
                    used_ignored.insert(block.content.clone());
                }
                blocks.push(BlockEval { block, matches, ignored_as, candidates: vec![], formerly: vec![] });
            }
        }
        assemblies.push(AssemblyEval { assembly, blocks });
    }

    let stale_ignored: Vec<(String, String)> = ignored
        .entries()
        .filter(|(_, c)| !used_ignored.contains(*c))
        .map(|(r, c)| (r.to_string(), c.to_string()))
        .collect();
    let unused = (0..scan.marked.len()).filter(|i| !matched[*i]).collect();

    if with_candidates {
        let files: HashMap<&str, &repo::RepoFile> = scan.files.iter().map(|f| (f.file.as_str(), f)).collect();
        let pool = candidate_pool(&scan, &files);
        for a in &mut assemblies {
            for b in a.blocks.iter_mut().filter(|b| !b.done()) {
                b.candidates = candidates_for(&b.block, &pool);
                b.formerly = stale_ignored
                    .iter()
                    .map(|(r, c)| Formerly { reason: r.clone(), content: c.clone(), similarity: similarity(&b.block.content, c) })
                    .filter(|f| f.similarity >= 0.5)
                    .max_by(|x, y| x.similarity.total_cmp(&y.similarity))
                    .into_iter()
                    .collect();
            }
        }
    }

    Ok(Evaluation { assemblies, scan, stale_ignored, unused })
}

struct PoolEntry<'a> {
    id: String,
    cand: Candidate<'a>,
    tokens: Vec<String>,
}

fn candidate_pool<'a>(scan: &'a Scan, files: &HashMap<&str, &'a repo::RepoFile>) -> Vec<PoolEntry<'a>> {
    let mut pool: Vec<PoolEntry> = scan
        .marked
        .iter()
        .filter_map(|code| {
            let f = files.get(code.file.as_str())?;
            Some(PoolEntry {
                id: code.id.clone(),
                tokens: tokens(&code.content),
                cand: Candidate { file: &code.file, section: code.section.as_deref(), marked: Some(code), text: &f.text, markers: &f.markers },
            })
        })
        .collect();
    for f in scan.files.iter().filter(|f| f.markers.file.is_none()) {
        pool.push(PoolEntry {
            id: f.file.clone(),
            tokens: tokens(&f.text),
            cand: Candidate { file: &f.file, section: None, marked: None, text: &f.text, markers: &f.markers },
        });
    }
    pool
}

fn candidates_for(block: &Block, pool: &[PoolEntry]) -> Vec<CandidateInfo> {
    let plain = if block.content.lines().any(|l| l.starts_with("$ ")) {
        markers::apply_options(&block.content, &[lightbulb::prompt_option()]).unwrap_or_else(|_| block.content.clone())
    } else {
        block.content.clone()
    };
    let plain_tokens = tokens(&plain);
    let mut scored: Vec<CandidateInfo> = Vec::new();
    for e in pool {
        let plan = lightbulb::plan_for(block, &e.cand);
        let code: Option<&MarkedCode> = e.cand.marked;
        let doc_options = match code {
            Some(c) if !c.doc_options.is_empty() => c.doc_options.clone(),
            _ => plan.as_ref().map(|p| p.doc_options.clone()).unwrap_or_default(),
        };
        let doc = doc_side(block, &doc_options).unwrap_or_else(|| block.content.clone());
        let mark_section = plan.as_ref().and_then(|p| {
            p.fixes.iter().find_map(|f| match f {
                Fix::MarkSection { line, lines, .. } => Some((*line, *lines)),
                _ => None,
            })
        });
        let content = match (mark_section, code) {
            (Some((line, count)), _) => e.cand.text.split('\n').skip(line - 1).take(count).collect::<Vec<_>>().join("\n") + "\n",
            (None, Some(c)) => c.matcher.fill(&doc),
            (None, None) => e.cand.text.to_string(),
        };
        let sim = if plan.is_some() {
            1.0
        } else {
            let d = dice(&plain_tokens, &e.tokens);
            if d < MIN_SIMILARITY {
                continue;
            }
            let s = 0.5 * d + 0.5 * line_ratio(&plain, &content);
            if s < MIN_SIMILARITY {
                continue;
            }
            s
        };
        scored.push(CandidateInfo {
            id: e.id.clone(),
            kind: match (mark_section, code) {
                (Some(_), _) => "lines",
                (None, Some(c)) if c.section.is_some() => "section",
                (None, Some(_)) => "file",
                (None, None) => "unmarked-file",
            },
            file: e.cand.file.to_string(),
            name: e.cand.section.map(str::to_string),
            lines: mark_section.map(|(l, n)| (l, l + n - 1)).or(code.and_then(|c| c.lines)),
            marker_lines: code.map(|c| c.marker_lines.clone()).unwrap_or_default(),
            options: code.map(|c| c.options.clone()).unwrap_or_default(),
            doc_options,
            placeholders: code.map(|c| c.placeholders.clone()).unwrap_or_default(),
            plan,
            similarity: sim,
            content,
            doc,
            file_text: e.cand.text.to_string(),
        });
    }
    scored.sort_by(|x, y| y.plan.is_some().cmp(&x.plan.is_some()).then(y.similarity.total_cmp(&x.similarity)));
    scored.truncate(MAX_CANDIDATES);
    scored
}
