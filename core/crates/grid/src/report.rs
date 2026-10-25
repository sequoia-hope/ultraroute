//! What the router is doing, as it does it, for anyone watching.

use std::sync::Arc;

use board::{Board, ItemId, Kind, Point};

/// Where the router is looking.
#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    Connection { net: u32, from: Point, to: Point },
    Part(usize),
    /// A part being tried `by` this far from where it stands, before the move is judged.
    Trial { part: usize, by: Point },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub phase: &'static str,
    pub text: String,
    pub focus: Option<Focus>,
}

/// Receives the router's activity.
#[derive(Clone)]
pub struct Log(pub Arc<dyn Fn(Activity) + Send + Sync>);

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Log")
    }
}

pub(crate) fn tell(log: &Option<Log>, phase: &'static str, text: impl FnOnce() -> String, focus: Option<Focus>) {
    if let Some(Log(log)) = log {
        log(Activity { phase, text: text(), focus });
    }
}

/// An item as a person would name it: a pad by part and number, otherwise its kind.
pub fn item_name(board: &Board, id: ItemId) -> String {
    match board.item(id).map(|it| &it.kind) {
        Some(Kind::Pad { footprint, number, .. }) => format!("{}-{number}", board.footprints[*footprint].reference),
        Some(Kind::Track) => "a track".to_string(),
        Some(Kind::Via) => "a via".to_string(),
        Some(_) => "copper".to_string(),
        None => "gone".to_string(),
    }
}
