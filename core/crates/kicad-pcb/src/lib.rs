//! `.kicad_pcb` reader and writer, and the `.kicad_pro`/`.kicad_dru` rules.
//!
//! ```text
//! text ──sexpr::parse──▶ Node ──read::read──▶ Pcb ──outline::chain──▶ OutlineLoops
//!                             ──copper::read_copper──▶ Copper
//! ```
//!
//! What comes out is a neutral record in metres (Y down, as the file is)
//! plus the board outline chained into one outer loop and its cutouts.
//! Everything the reader does not understand is counted into one warning;
//! everything that would make the board silently wrong is a typed error.
//! `write::apply` puts edits back into the text and keeps every other byte.
//!
//! No workspace dependency: the reader began as waffle-iron's (same
//! author), and the router's `board` crate builds its model from this one.

pub mod copper;
pub mod model;
pub mod outline;
pub mod read;
pub mod rules;
pub mod sexpr;
pub mod write;

pub use copper::{read_copper, ArcTrack, Copper, CopperLayer, Group, Keepout, Outline, PadCopper, PadShape, Span, Track, Via, Zone};
pub use model::*;
pub use outline::{
    arc_is_ccw, chain, circumcenter, Loop, OutlineError, OutlineLoops, Segment, OUTLINE_WELD_M,
};
pub use read::MIN_VERSION;
pub use rules::{NetClass, Rules, RulesError};
pub use write::{apply, mm, Edit, NewTrack, NewVia};

/// Why a file could not be read into a [`Pcb`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KicadParse {
    #[error("not a kicad_pcb file (top-level form is `{found}`)")]
    NotAKicadPcb { found: String },
    #[error("kicad_pcb version {found} is older than the supported minimum {min} (KiCad 6)")]
    UnsupportedVersion { found: u32, min: u32 },
    #[error("syntax error at line {line}, column {col}: expected {expected}")]
    Syntax {
        line: u32,
        col: u32,
        expected: String,
    },
    #[error("board thickness missing: neither (general (thickness …)) nor a stackup with layer thicknesses")]
    MissingThickness,
    #[error("two footprints share the uuid {uuid}")]
    DuplicateFootprintUuid { uuid: String },
}

/// Read a `.kicad_pcb` text.
pub fn parse_kicad_pcb(text: &str) -> Result<Pcb, KicadParse> {
    let mut root = sexpr::parse(text)?;
    sexpr::number_nets(&mut root);
    read::read(&root)
}

/// The board and the copper a router works with, from one parse.
pub fn parse_board(text: &str) -> Result<(Pcb, Copper), KicadParse> {
    let mut root = sexpr::parse(text)?;
    sexpr::number_nets(&mut root);
    let pcb = read::read(&root)?;
    let copper = copper::read_copper(&root, &pcb.footprints)?;
    Ok((pcb, copper))
}

impl Pcb {
    /// The Edge.Cuts primitives chained into the board loop and its
    /// cutouts.
    pub fn outline_loops(&self) -> Result<OutlineLoops, OutlineError> {
        outline::chain(&self.outline)
    }
}
