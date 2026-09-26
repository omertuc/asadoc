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

const HEADING: &str = r"^=+\s+(.+)";
const SOURCE: &str = r"^\[source,\s*(\w+)";
const INCLUDE_MODULE: &str = r"(?m)^include::modules/([\w.-]+)\.adoc\[";
const TITLE: &str = r"(?m)^=\s+(.+)$";
const LIST_MARKER: &str = r"^(\.+|\*+)\s+";
const BLOCK_TITLE: &str = r"^\.(\S)";
const CONDITIONAL: &str = r"^(ifdef|ifndef|endif)::";
const SPACES: &str = r"\s+";

pub(crate) fn format_ref(module: &str, lang: &str, seq: usize) -> String {
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
    let file_name = Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = match regex!(TITLE)?.captures(&text) {
        Some(captures) => group(&captures, 1)?.trim().to_owned(),
        None => file_name.clone(),
    };
    Ok(Assembly {
        id: file_name,
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
fn is_filler(line: &str, conditional: &Regex) -> bool {
    line.is_empty() || line == "+" || line.starts_with('[') || line.starts_with("//") || conditional.is_match(line)
}

fn lead_for(lines: &[&str]) -> Result<Option<String>> {
    let (heading, conditional) = (regex!(HEADING)?, regex!(CONDITIONAL)?);
    let Some(line) = lines
        .iter()
        .rev()
        .map(|line| line.trim())
        .find(|line| !is_filler(line, conditional))
    else {
        return Ok(None);
    };
    if line.starts_with("----") || line.starts_with("....") || heading.is_match(line) {
        return Ok(None);
    }
    let unlisted = regex!(LIST_MARKER)?.replace(line, "");
    let untitled = regex!(BLOCK_TITLE)?.replace(&unlisted, "$1");
    let lead = regex!(SPACES)?.replace_all(&untitled, " ").trim().to_owned();
    Ok((!lead.is_empty()).then_some(lead))
}

/// The next 1-based position for a block in `lang`
fn next_seq(seq_by_lang: &mut BTreeMap<String, usize>, lang: &str) -> usize {
    let count = seq_by_lang.entry(lang.to_owned()).or_insert(0);
    *count += 1;
    *count
}

/// A module's code blocks; none when the module doesn't exist
pub(crate) fn extract_blocks(docs: &Docs, module: &str) -> Result<Vec<Block>> {
    let Some(text) = docs
        .read(&module_path(module))
        .with_context(|| format!("reading the module {module}"))?
    else {
        return Ok(vec![]);
    };
    let (heading, source) = (regex!(HEADING)?, regex!(SOURCE)?);
    let lines: Vec<&str> = text.split('\n').collect();
    let is_delimiter = |index: &usize| lines.get(*index).is_some_and(|line| line.starts_with("----"));
    let mut blocks = Vec::new();
    let mut seq_by_lang = BTreeMap::new();
    let mut section = None;
    let mut index = 0;
    while let Some(line) = lines.get(index) {
        if let Some(heading_captures) = heading.captures(line) {
            section = Some(group(&heading_captures, 1)?.trim().to_owned());
        }
        if let Some(source_captures) = source.captures(line) {
            let lang = group(&source_captures, 1)?.to_owned();
            let lead = lead_for(lines.get(..index).unwrap_or_default())
                .with_context(|| format!("finding the text introducing the block on line {}", index + 1))?;
            let Some(open) = (index + 1..lines.len()).find(is_delimiter) else {
                break;
            };
            let start = open + 1;
            let close = (start..lines.len()).find(is_delimiter).unwrap_or(lines.len());
            let seq = next_seq(&mut seq_by_lang, &lang);
            blocks.push(Block {
                module: module.to_owned(),
                reference: format_ref(module, &lang, seq),
                lang,
                seq,
                content: lines.get(start..close).unwrap_or_default().join("\n") + "\n",
                line: start + 1,
                section: section.clone(),
                lead,
            });
            index = close;
        }
        index += 1;
    }
    Ok(blocks)
}

// ---------------------------------------------------------------------------
// Rendering support: the attributes a module needs when rendered on its own
// ---------------------------------------------------------------------------

const ATTRIBUTE: &str = r"^:([\w-]+):\s*(.*)$";
const ATTRIBUTE_REF: &str = r"\{([\w-]+)\}";
const CONDITIONAL_BLOCK: &str = r"^(ifdef|ifndef|endif)::([\w,+-]*)\[\]\s*$";
const ENTERPRISE_BRANCH: &str = r"^enterprise-(\d+)\.(\d+)$";

/// Attributes a docs build would provide that don't make sense standalone
const PRESENTATION: &[&str] = &["data-uri", "icons", "imagesdir", "toc", "toc-title", "experimental"];

/// Asciidoctor's built-in character replacement attributes
const BUILT_IN: &[(&str, &str)] = &[("nbsp", "\u{a0}"), ("zwsp", "\u{200b}"), ("empty", ""), ("sp", " ")];

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
    let mut attrs: BTreeMap<String, String> = distro_attributes(docs)
        .context("reading the product attributes")?
        .into_iter()
        .collect();
    let text = docs
        .read(assembly)
        .with_context(|| format!("reading the assembly {assembly}"))?
        .with_context(|| format!("assembly {assembly} not found in the docs"))?;
    // Honor ifdef/ifndef blocks the way a docs build for the distro
    // (openshift-enterprise) would
    attrs.insert("openshift-enterprise".to_owned(), String::new());
    add_header_attributes(docs, assembly, &text, &mut attrs)?;
    attrs.retain(|name, _| !PRESENTATION.contains(&name.as_str()));
    resolve_references(attrs).context("resolving attribute references")
}

/// Product title and version, from `_distro_map.yml`
fn distro_attributes(docs: &Docs) -> Result<Vec<(String, String)>> {
    let Some(distro) = docs.read("_distro_map.yml").context("reading _distro_map.yml")? else {
        return Ok(vec![]);
    };
    let distro: Value = serde_yaml::from_str(&distro).context("parsing _distro_map.yml")?;
    let enterprise = distro.get("openshift-enterprise");
    let product_title = enterprise
        .and_then(|enterprise| enterprise.get("name"))
        .and_then(Value::as_str)
        .map(|name| ("product-title".to_owned(), name.to_owned()));
    let product_version = enterprise
        .and_then(|enterprise| enterprise.get("branches"))
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
fn add_header_attributes(docs: &Docs, assembly: &str, text: &str, attrs: &mut BTreeMap<String, String>) -> Result<()> {
    let attribute = regex!(ATTRIBUTE)?;
    for line in text.lines().take_while(|line| !line.starts_with("include::modules/")) {
        if let Some(rest) = line.strip_prefix("include::") {
            if let Some(file) = rest.split('[').next().filter(|file| file.starts_with("_attributes/")) {
                let included = docs
                    .read(file)
                    .with_context(|| format!("reading {file}"))?
                    .unwrap_or_default();
                add_attribute_entries(&included, attrs).with_context(|| format!("reading the attributes in {file}"))?;
            }
        } else if attribute.is_match(line) {
            add_attribute_entries(line, attrs).with_context(|| format!("reading the attributes in {assembly}"))?;
        }
    }
    Ok(())
}

/// Adds the attribute entries in `text`, skipping those in ifdef/ifndef blocks
/// whose condition doesn't hold
fn add_attribute_entries(text: &str, attrs: &mut BTreeMap<String, String>) -> Result<()> {
    let (conditional_block, attribute) = (regex!(CONDITIONAL_BLOCK)?, regex!(ATTRIBUTE)?);
    let mut active: Vec<bool> = Vec::new();
    for line in text.lines() {
        if let Some(captures) = conditional_block.captures(line) {
            let defined = group(&captures, 2)?
                .split([',', '+'])
                .any(|name| attrs.contains_key(name));
            match group(&captures, 1)? {
                "ifdef" => active.push(defined),
                "ifndef" => active.push(!defined),
                _ => {
                    active.pop();
                }
            }
            continue;
        }
        if active.iter().all(|is_active| *is_active)
            && let Some(captures) = attribute.captures(line)
        {
            attrs.insert(group(&captures, 1)?.to_owned(), group(&captures, 2)?.to_owned());
        }
    }
    Ok(())
}

/// Replaces references to other attributes (and Asciidoctor's built-in
/// character replacements) with their values, as a docs build would
fn resolve_references(attrs: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let attribute_ref = regex!(ATTRIBUTE_REF)?;
    Ok((0..RESOLVE_PASSES).fold(attrs, |attrs, _| resolve_once(attribute_ref, &attrs)))
}

/// One pass of [`resolve_references`]: each reference replaced by the value it had before the pass
fn resolve_once(attribute_ref: &Regex, attrs: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let known: BTreeMap<&str, &str> = BUILT_IN
        .iter()
        .copied()
        .chain(attrs.iter().map(|(name, value)| (name.as_str(), value.as_str())))
        .collect();
    attrs
        .iter()
        .map(|(name, value)| {
            let resolved = attribute_ref.replace_all(value, |captures: &Captures<'_>| {
                let whole = captures.get(0).map_or("", |reference| reference.as_str());
                captures
                    .get(1)
                    .and_then(|referenced| known.get(referenced.as_str()))
                    .copied()
                    .unwrap_or(whole)
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
