//! A hook diagnostics can set to look at the board between steps of a stage ([`mark`]); unset,
//! marking costs nothing.

use std::sync::OnceLock;

use crate::model::Board;

type Hook = Box<dyn Fn(&str, &Board) + Send + Sync>;

static HOOK: OnceLock<Hook> = OnceLock::new();

/// Sets the hook, once per process.
pub fn set(hook: Hook) {
    let _ = HOOK.set(hook);
}

/// The board as it stands after the step `label`.
pub fn mark(label: &str, board: &Board) {
    if let Some(h) = HOOK.get() {
        h(label, board);
    }
}
