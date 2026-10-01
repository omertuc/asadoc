//! What asadoc is busy with, so `asadoc serve` can show it on a page opened
//! before the review UI is ready.

use std::sync::{Mutex, PoisonError};

static CURRENT_STEP: Mutex<String> = Mutex::new(String::new());

/// Records the step asadoc is on
pub(crate) fn step(description: impl Into<String>) {
    *CURRENT_STEP.lock().unwrap_or_else(PoisonError::into_inner) = description.into();
}

/// Records the step asadoc is on, and tells the terminal too (for steps slow
/// enough to wonder about, like fetching)
pub(crate) fn announce(description: impl Into<String>) {
    let description = description.into();
    eprintln!("asadoc: {description}");
    step(description);
}

/// The step asadoc is on (empty before the first)
pub(crate) fn current_step() -> String {
    CURRENT_STEP.lock().unwrap_or_else(PoisonError::into_inner).clone()
}
