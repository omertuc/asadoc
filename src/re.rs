//! Regexes compiled once, and reading their capture groups, without panicking.

use anyhow::{Context, Result};
use regex::Captures;

/// A regex compiled once, on first use. Evaluates to `anyhow::Result<&'static Regex>`,
/// so a bad pattern is an error rather than a panic.
macro_rules! regex {
    ($pattern:expr) => {{
        static RE: std::sync::LazyLock<Result<regex::Regex, regex::Error>> =
            std::sync::LazyLock::new(|| regex::Regex::new($pattern));
        anyhow::Context::with_context(RE.as_ref().map_err(Clone::clone), || {
            format!("compiling the regex {:?}", $pattern)
        })
    }};
}

/// The text of capture group `index`
pub(crate) fn group<'t>(captures: &Captures<'t>, index: usize) -> Result<&'t str> {
    captures
        .get(index)
        .map(|matched| matched.as_str())
        .with_context(|| format!("regex group {index} didn't participate in the match"))
}
