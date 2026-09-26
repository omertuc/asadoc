//! The doc side: code blocks in the modules an `AsciiDoc` assembly includes.

use crate::re::group;
use crate::source::Docs;
use anyhow::{Context, Result};
use regex::{Captures, Regex};
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// A `[source,…]` block in a module
#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub module: String,
    pub lang: String,
    /// 1-based position among the module's blocks of the same language
    pub seq: usize,
    /// `<module>/<lang>-<NNN>`: a name for the block's current position only
    pub reference: String,
    /// The lines between the `----` delimiters, ending with a newline
    pub content: String,
    /// 1-based line of the first content line in the module
    pub line: usize,
    /// The heading the block is under
    pub section: Option<String>,
    /// The text introducing the block: the nearest procedure step, block title or prose line above it
    pub lead: Option<String>,
}

pub(crate) struct Assembly {
    pub id: String,
    /// Relative to the docs root
    pub path: String,
    pub title: String,
    pub modules: Vec<String>,
}

const HEADING_LINE: &str = r"^=+\s+(.+)";
const SOURCE_BLOCK_START: &str = r"^\[source,\s*(\w+)";
const INCLUDE_MODULE: &str = r"(?m)^include::modules/([\w.-]+)\.adoc\[";
const DOCUMENT_TITLE: &str = r"(?m)^=\s+(.+)$";
const LIST_MARKER: &str = r"^(\.+|\*+)\s+";
const BLOCK_TITLE: &str = r"^\.(\S)";
const CONDITIONAL_DIRECTIVE: &str = r"^(ifdef|ifndef|endif)::";
const WHITESPACE_RUN: &str = r"\s+";

pub(crate) fn format_reference(module: &str, lang: &str, seq: usize) -> String {
    format!("{module}/{lang}-{seq:03}")
}

pub(crate) fn read_assembly(docs: &Docs, path: &str) -> Result<Assembly> {
    let text = docs
        .read(path)
        .with_context(|| format!("reading the assembly {path}"))?
        .with_context(|| format!("assembly {path} not found in the docs"))?;
    let included = regex!(INCLUDE_MODULE)?
        .captures_iter(&text)
        .map(|captures| group(&captures, 1))
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("finding the modules {path} includes"))?;
    let file_stem = Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = match regex!(DOCUMENT_TITLE)?.captures(&text) {
        Some(captures) => group(&captures, 1)?.trim().to_owned(),
        None => file_stem.clone(),
    };
    Ok(Assembly {
        id: file_stem,
        path: path.to_owned(),
        title,
        modules: unique_in_order(included),
    })
}

/// The distinct items, each where it first appears
fn unique_in_order(items: Vec<&str>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(*item))
        .map(str::to_owned)
        .collect()
}

pub(crate) fn module_path(module: &str) -> String {
    format!("modules/{module}.adoc")
}

/// Lines that say nothing about the block below them
fn is_filler(line: &str, conditional_directive: &Regex) -> bool {
    line.is_empty()
        || line == "+"
        || line.starts_with('[')
        || line.starts_with("//")
        || conditional_directive.is_match(line)
}

fn lead_for(lines_above: &[&str]) -> Result<Option<String>> {
    let (heading_line, conditional_directive) = (regex!(HEADING_LINE)?, regex!(CONDITIONAL_DIRECTIVE)?);
    let Some(nearest_line) = lines_above
        .iter()
        .rev()
        .map(|line| line.trim())
        .find(|line| !is_filler(line, conditional_directive))
    else {
        return Ok(None);
    };
    if nearest_line.starts_with("----") || nearest_line.starts_with("....") || heading_line.is_match(nearest_line) {
        return Ok(None);
    }
    let without_list_marker = regex!(LIST_MARKER)?.replace(nearest_line, "");
    let without_title_dot = regex!(BLOCK_TITLE)?.replace(&without_list_marker, "$1");
    let lead = regex!(WHITESPACE_RUN)?
        .replace_all(&without_title_dot, " ")
        .trim()
        .to_owned();
    Ok((!lead.is_empty()).then_some(lead))
}

/// The next 1-based position for a block in `lang`
fn next_seq(seq_by_lang: &mut BTreeMap<String, usize>, lang: &str) -> usize {
    let last_seq = seq_by_lang.entry(lang.to_owned()).or_insert(0);
    *last_seq += 1;
    *last_seq
}

/// A module's code blocks; none when the module doesn't exist
pub(crate) fn extract_blocks(docs: &Docs, module: &str) -> Result<Vec<Block>> {
    let Some(text) = docs
        .read(&module_path(module))
        .with_context(|| format!("reading the module {module}"))?
    else {
        return Ok(vec![]);
    };
    let (heading_line, source_block_start) = (regex!(HEADING_LINE)?, regex!(SOURCE_BLOCK_START)?);
    let lines: Vec<&str> = text.split('\n').collect();
    let is_delimiter = |line_index: &usize| lines.get(*line_index).is_some_and(|line| line.starts_with("----"));
    let mut blocks = Vec::new();
    let mut seq_by_lang = BTreeMap::new();
    let mut section = None;
    let mut line_index = 0;
    while let Some(line) = lines.get(line_index) {
        if let Some(heading_captures) = heading_line.captures(line) {
            section = Some(group(&heading_captures, 1)?.trim().to_owned());
        }
        if let Some(source_captures) = source_block_start.captures(line) {
            let lang = group(&source_captures, 1)?.to_owned();
            let lead = lead_for(lines.get(..line_index).unwrap_or_default())
                .with_context(|| format!("finding the text introducing the block on line {}", line_index + 1))?;
            let Some(open_delimiter) = (line_index + 1..lines.len()).find(is_delimiter) else {
                break;
            };
            let content_start = open_delimiter + 1;
            let close_delimiter = (content_start..lines.len()).find(is_delimiter).unwrap_or(lines.len());
            let seq = next_seq(&mut seq_by_lang, &lang);
            blocks.push(Block {
                module: module.to_owned(),
                reference: format_reference(module, &lang, seq),
                lang,
                seq,
                content: lines.get(content_start..close_delimiter).unwrap_or_default().join("\n") + "\n",
                line: content_start + 1,
                section: section.clone(),
                lead,
            });
            line_index = close_delimiter;
        }
        line_index += 1;
    }
    Ok(blocks)
}

// ---------------------------------------------------------------------------
// Rendering support: the attributes a module needs when rendered on its own
// ---------------------------------------------------------------------------

const ATTRIBUTE_ENTRY: &str = r"^:([\w-]+):\s*(.*)$";
const ATTRIBUTE_REFERENCE: &str = r"\{([\w-]+)\}";
const CONDITIONAL_BLOCK: &str = r"^(ifdef|ifndef|endif)::([\w,+-]*)\[\]\s*$";
const ENTERPRISE_BRANCH: &str = r"^enterprise-(\d+)\.(\d+)$";

/// Attributes a docs build would provide that don't make sense standalone
const PRESENTATION_ATTRIBUTES: &[&str] = &["data-uri", "icons", "imagesdir", "toc", "toc-title", "experimental"];

/// Asciidoctor's built-in character replacement attributes
const BUILT_IN_REPLACEMENTS: &[(&str, &str)] = &[("nbsp", "\u{a0}"), ("zwsp", "\u{200b}"), ("empty", ""), ("sp", " ")];

/// How many levels of attributes referring to other attributes get resolved
const RESOLVE_PASSES: usize = 5;

/// Attributes for rendering a module of `assembly` standalone: product title and
/// version (from `_distro_map.yml`, like `AsciiBinder`), the attribute files the
/// assembly includes, and the assembly's own header entries.
pub(crate) fn assembly_attributes(docs: &Docs, assembly: &str) -> Result<BTreeMap<String, String>> {
    docs.prefetch(&[
        "_distro_map.yml".to_owned(),
        "_attributes".to_owned(),
        assembly.to_owned(),
    ])
    .context("prefetching the attribute files")?;
    let mut attributes: BTreeMap<String, String> = distro_attributes(docs)
        .context("reading the product attributes")?
        .into_iter()
        .collect();
    let text = docs
        .read(assembly)
        .with_context(|| format!("reading the assembly {assembly}"))?
        .with_context(|| format!("assembly {assembly} not found in the docs"))?;
    // Honor ifdef/ifndef blocks the way a docs build for the distro
    // (openshift-enterprise) would
    attributes.insert("openshift-enterprise".to_owned(), String::new());
    add_header_attributes(docs, assembly, &text, &mut attributes)?;
    attributes.retain(|name, _| !PRESENTATION_ATTRIBUTES.contains(&name.as_str()));
    resolve_references(attributes).context("resolving attribute references")
}

/// Product title and version, from `_distro_map.yml`
fn distro_attributes(docs: &Docs) -> Result<Vec<(String, String)>> {
    let Some(distro_map_text) = docs.read("_distro_map.yml").context("reading _distro_map.yml")? else {
        return Ok(vec![]);
    };
    let distro_map: Value = serde_yaml::from_str(&distro_map_text).context("parsing _distro_map.yml")?;
    let enterprise_distro = distro_map.get("openshift-enterprise");
    let product_title = enterprise_distro
        .and_then(|enterprise_distro| enterprise_distro.get("name"))
        .and_then(Value::as_str)
        .map(|name| ("product-title".to_owned(), name.to_owned()));
    let product_version = enterprise_distro
        .and_then(|enterprise_distro| enterprise_distro.get("branches"))
        .and_then(Value::as_mapping)
        .map(latest_enterprise_version)
        .transpose()
        .context("finding the latest enterprise branch")?
        .flatten()
        .map(|(major, minor)| ("product-version".to_owned(), format!("{major}.{minor}")));
    Ok(product_title.into_iter().chain(product_version).collect())
}

/// The highest `(major, minor)` among the `enterprise-<major>.<minor>` branches
fn latest_enterprise_version(branches: &Mapping) -> Result<Option<(u32, u32)>> {
    let enterprise_branch = regex!(ENTERPRISE_BRANCH)?;
    let versions = branches
        .keys()
        .filter_map(Value::as_str)
        .filter_map(|branch| enterprise_branch.captures(branch))
        .map(|captures| -> Result<(u32, u32)> {
            Ok((
                group(&captures, 1)?
                    .parse()
                    .context("parsing a branch's major version")?,
                group(&captures, 2)?
                    .parse()
                    .context("parsing a branch's minor version")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(versions.into_iter().max())
}

/// The attribute entries above the assembly's first module, and those in the
/// attribute files it includes there
fn add_header_attributes(
    docs: &Docs,
    assembly: &str,
    text: &str,
    attributes: &mut BTreeMap<String, String>,
) -> Result<()> {
    let attribute_entry = regex!(ATTRIBUTE_ENTRY)?;
    for line in text.lines().take_while(|line| !line.starts_with("include::modules/")) {
        if let Some(include_target) = line.strip_prefix("include::") {
            if let Some(attribute_file) = include_target
                .split('[')
                .next()
                .filter(|attribute_file| attribute_file.starts_with("_attributes/"))
            {
                let attribute_file_text = docs
                    .read(attribute_file)
                    .with_context(|| format!("reading {attribute_file}"))?
                    .unwrap_or_default();
                add_attribute_entries(&attribute_file_text, attributes)
                    .with_context(|| format!("reading the attributes in {attribute_file}"))?;
            }
        } else if attribute_entry.is_match(line) {
            add_attribute_entries(line, attributes).with_context(|| format!("reading the attributes in {assembly}"))?;
        }
    }
    Ok(())
}

/// Adds the attribute entries in `text`, skipping those in ifdef/ifndef blocks
/// whose condition doesn't hold
fn add_attribute_entries(text: &str, attributes: &mut BTreeMap<String, String>) -> Result<()> {
    let (conditional_block, attribute_entry) = (regex!(CONDITIONAL_BLOCK)?, regex!(ATTRIBUTE_ENTRY)?);
    let mut enclosing_conditions: Vec<bool> = Vec::new();
    for line in text.lines() {
        if let Some(captures) = conditional_block.captures(line) {
            let any_defined = group(&captures, 2)?
                .split([',', '+'])
                .any(|name| attributes.contains_key(name));
            match group(&captures, 1)? {
                "ifdef" => enclosing_conditions.push(any_defined),
                "ifndef" => enclosing_conditions.push(!any_defined),
                _ => {
                    enclosing_conditions.pop();
                }
            }
            continue;
        }
        if enclosing_conditions.iter().all(|is_active| *is_active)
            && let Some(captures) = attribute_entry.captures(line)
        {
            attributes.insert(group(&captures, 1)?.to_owned(), group(&captures, 2)?.to_owned());
        }
    }
    Ok(())
}

/// Replaces references to other attributes (and Asciidoctor's built-in
/// character replacements) with their values, as a docs build would
fn resolve_references(attributes: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let attribute_reference = regex!(ATTRIBUTE_REFERENCE)?;
    Ok((0..RESOLVE_PASSES).fold(attributes, |attributes, _| {
        resolve_once(attribute_reference, &attributes)
    }))
}

/// One pass of [`resolve_references`]: each reference replaced by the value it had before the pass
fn resolve_once(attribute_reference: &Regex, attributes: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let known_values: BTreeMap<&str, &str> = BUILT_IN_REPLACEMENTS
        .iter()
        .copied()
        .chain(attributes.iter().map(|(name, value)| (name.as_str(), value.as_str())))
        .collect();
    attributes
        .iter()
        .map(|(name, value)| {
            let resolved = attribute_reference.replace_all(value, |captures: &Captures<'_>| {
                let whole_reference = captures.get(0).map_or("", |reference| reference.as_str());
                captures
                    .get(1)
                    .and_then(|referenced_name| known_values.get(referenced_name.as_str()))
                    .copied()
                    .unwrap_or(whole_reference)
                    .to_owned()
            });
            (name.clone(), resolved.into_owned())
        })
        .collect()
}

/// A module's text for rendering: its `//` comment lines removed; None when there's no such module
pub(crate) fn module_for_rendering(docs: &Docs, module: &str) -> Result<Option<String>> {
    let text = docs
        .read(&module_path(module))
        .with_context(|| format!("reading the module {module}"))?;
    Ok(text.map(|text| {
        text.split_inclusive('\n')
            .filter(|line| !line.starts_with("//"))
            .collect()
    }))
}
