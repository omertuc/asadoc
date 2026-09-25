//! The doc side: code blocks in the modules an AsciiDoc assembly includes.

use regex::Regex;
use std::collections::BTreeMap;
use crate::source::Docs;
use std::path::Path;
use std::sync::LazyLock;

/// A `[source,…]` block in a module
#[derive(Clone, Debug)]
pub struct Block {
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

pub struct Assembly {
    pub id: String,
    pub title: String,
    pub modules: Vec<String>,
}

static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^=+\s+(.+)").unwrap());
static SOURCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[source,\s*(\w+)").unwrap());
static INCLUDE_MODULE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^include::modules/([\w.-]+)\.adoc\[").unwrap());
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^=\s+(.+)$").unwrap());
static LIST_MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\.+|\*+)\s+").unwrap());
static BLOCK_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\.(\S)").unwrap());
static CONDITIONAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(ifdef|ifndef|endif)::").unwrap());
static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

pub fn format_ref(module: &str, lang: &str, seq: usize) -> String {
    format!("{module}/{lang}-{seq:03}")
}

pub fn read_assembly(docs: &Docs, path: &str) -> Assembly {
    let text = docs.read(path).unwrap_or_default();
    let mut modules: Vec<String> = Vec::new();
    for c in INCLUDE_MODULE.captures_iter(&text) {
        if !modules.iter().any(|m| m == &c[1]) {
            modules.push(c[1].to_string());
        }
    }
    let file_name = Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Assembly {
        id: file_name.clone(),
        title: TITLE.captures(&text).map(|c| c[1].trim().to_string()).unwrap_or(file_name),
        modules,
    }
}

pub fn module_path(module: &str) -> String {
    format!("modules/{module}.adoc")
}

fn lead_for(lines: &[&str], source_line: usize) -> Option<String> {
    for j in (0..source_line).rev() {
        let l = lines[j].trim();
        if l.is_empty() || l == "+" || l.starts_with('[') || l.starts_with("//") || CONDITIONAL.is_match(l) {
            continue;
        }
        if l.starts_with("----") || l.starts_with("....") || HEADING.is_match(l) {
            return None;
        }
        let l = LIST_MARKER.replace(l, "");
        let l = BLOCK_TITLE.replace(&l, "$1");
        let l = SPACES.replace_all(&l, " ").trim().to_string();
        return (!l.is_empty()).then_some(l);
    }
    None
}

pub fn extract_blocks(docs: &Docs, module: &str) -> Vec<Block> {
    let Some(text) = docs.read(&module_path(module)) else { return vec![] };
    let lines: Vec<&str> = text.split('\n').collect();
    let mut blocks = Vec::new();
    let mut seq_by_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut section = None;
    let mut i = 0;
    while i < lines.len() {
        if let Some(h) = HEADING.captures(lines[i]) {
            section = Some(h[1].trim().to_string());
        }
        if let Some(m) = SOURCE.captures(lines[i]) {
            let lang = m[1].to_string();
            let lead = lead_for(&lines, i);
            i += 1;
            while i < lines.len() && !lines[i].starts_with("----") {
                i += 1;
            }
            if i >= lines.len() {
                break;
            }
            i += 1;
            let start = i;
            while i < lines.len() && !lines[i].starts_with("----") {
                i += 1;
            }
            let seq = {
                let n = seq_by_lang.entry(lang.clone()).or_insert(0);
                *n += 1;
                *n
            };
            blocks.push(Block {
                module: module.to_string(),
                reference: format_ref(module, &lang, seq),
                lang,
                seq,
                content: lines[start..i.min(lines.len())].join("\n") + "\n",
                line: start + 1,
                section: section.clone(),
                lead,
            });
        }
        i += 1;
    }
    blocks
}

// ---------------------------------------------------------------------------
// Rendering support: the attributes a module needs when rendered on its own
// ---------------------------------------------------------------------------

static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^:([\w-]+):\s*(.*)$").unwrap());
static ATTRIBUTE_REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([\w-]+)\}").unwrap());
static CONDITIONAL_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(ifdef|ifndef|endif)::([\w,+-]*)\[\]\s*$").unwrap());
static ENTERPRISE_BRANCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^enterprise-(\d+)\.(\d+)$").unwrap());

/// Attributes a docs build would provide that don't make sense standalone
const PRESENTATION: &[&str] = &["data-uri", "icons", "imagesdir", "toc", "toc-title", "experimental"];

/// Attributes for rendering a module of `assembly` standalone: product title and
/// version (from `_distro_map.yml`, like AsciiBinder), the attribute files the
/// assembly includes, and the assembly's own header entries.
pub fn assembly_attributes(docs: &Docs, assembly: &str) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    docs.prefetch(&["_distro_map.yml".to_string(), "_attributes".to_string(), assembly.to_string()]);
    if let Some(distro) = docs
        .read("_distro_map.yml")
        .and_then(|t| serde_yaml::from_str::<serde_yaml::Value>(&t).ok())
    {
        let enterprise = &distro["openshift-enterprise"];
        if let Some(name) = enterprise["name"].as_str() {
            attrs.insert("product-title".to_string(), name.to_string());
        }
        if let Some(branches) = enterprise["branches"].as_mapping() {
            let latest = branches
                .keys()
                .filter_map(|k| k.as_str())
                .filter_map(|k| ENTERPRISE_BRANCH.captures(k).map(|c| (c[1].parse::<u32>().unwrap_or(0), c[2].parse::<u32>().unwrap_or(0))))
                .max();
            if let Some((major, minor)) = latest {
                attrs.insert("product-version".to_string(), format!("{major}.{minor}"));
            }
        }
    }
    let text = docs.read(assembly).unwrap_or_default();
    // Attribute entries, honoring ifdef/ifndef blocks the way a docs build for
    // the distro (openshift-enterprise) would
    attrs.insert("openshift-enterprise".to_string(), String::new());
    let add_from = |text: &str, attrs: &mut BTreeMap<String, String>| {
        let mut active: Vec<bool> = Vec::new();
        for line in text.lines() {
            if let Some(c) = CONDITIONAL_BLOCK.captures(line) {
                let names: Vec<&str> = c[2].split([',', '+']).collect();
                let defined = names.iter().any(|n| attrs.contains_key(*n));
                match &c[1] {
                    "ifdef" => active.push(defined),
                    "ifndef" => active.push(!defined),
                    _ => {
                        active.pop();
                    }
                }
                continue;
            }
            if active.iter().all(|a| *a) {
                if let Some(c) = ATTRIBUTE.captures(line) {
                    attrs.insert(c[1].to_string(), c[2].to_string());
                }
            }
        }
    };
    for line in text.lines() {
        if line.starts_with("include::modules/") {
            break;
        }
        if let Some(rest) = line.strip_prefix("include::") {
            if let Some(file) = rest.split('[').next().filter(|f| f.starts_with("_attributes/")) {
                add_from(&docs.read(file).unwrap_or_default(), &mut attrs);
            }
        } else if ATTRIBUTE.is_match(line) {
            add_from(line, &mut attrs);
        }
    }
    for p in PRESENTATION {
        attrs.remove(*p);
    }
    // Resolve references to other attributes (and Asciidoctor's built-in
    // character replacements), as a docs build would
    for _ in 0..5 {
        let mut snapshot = attrs.clone();
        for (name, value) in [("nbsp", "\u{a0}"), ("zwsp", "\u{200b}"), ("empty", ""), ("sp", " ")] {
            snapshot.entry(name.to_string()).or_insert_with(|| value.to_string());
        }
        for v in attrs.values_mut() {
            *v = ATTRIBUTE_REF
                .replace_all(v, |c: &regex::Captures| snapshot.get(&c[1]).cloned().unwrap_or_else(|| c[0].to_string()))
                .into_owned();
        }
    }
    attrs
}

/// A module's text for rendering: its `//` comment lines removed
pub fn module_for_rendering(docs: &Docs, module: &str) -> Option<String> {
    let text = docs.read(&module_path(module))?;
    Some(text.split_inclusive('\n').filter(|l| !l.starts_with("//")).collect())
}
