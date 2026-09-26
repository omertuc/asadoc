//! Regexes compiled once, and reading their capture groups, without panicking.

use anyhow::{Context, Result};
use regex::Captures;

/// A regex compiled once, on first use. Evaluates to `anyhow::Result<&'static Regex>`,
/// so a bad pattern is an error rather than a panic.
macro_rules! regex {
    ($pattern:expr) => {{
        static COMPILED_REGEX: std::sync::LazyLock<Result<regex::Regex, regex::Error>> =
            std::sync::LazyLock::new(|| regex::Regex::new($pattern));
        anyhow::Context::with_context(COMPILED_REGEX.as_ref().map_err(Clone::clone), || {
            format!("compiling the regex {:?}", $pattern)
        })
    }};
}

/// The text of capture group `group_index`
pub(crate) fn capture_group_text<'t>(captures: &Captures<'t>, group_index: usize) -> Result<&'t str> {
    captures
        .get(group_index)
        .map(|matched| matched.as_str())
        .with_context(|| format!("regex group {group_index} didn't participate in the match"))
}
