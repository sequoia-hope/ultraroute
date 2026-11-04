//! `.kicad_pcb` reader and writer, and the `.kicad_pro`/`.kicad_dru` rules.
//!
//! ```text
//! text ──sexpr::parse──▶ Node ──read::read──▶ Pcb ──outline::chain──▶ OutlineLoops
//!                             ──copper::read_copper──▶ Copper ──canon::canonical──▶ same, ordered
//! ```
//!
//! What comes out is a neutral record in metres (Y down, as the file is)
//! plus the board outline chained into one outer loop and its cutouts.
//! Everything the reader does not understand is counted into one warning;
//! everything that would make the board silently wrong is a typed error.
//! `canon::canonical` then orders it all by what it is, so a board reads the same whatever order
//! its file has.
//! `write::apply` puts edits back into the text and keeps every other byte.
//! `place::apply` moves, turns and flips footprints to the other side as pcbnew 9 does, byte
//! for byte, and also keeps every byte it does not move.
//!
//! No workspace dependency: the reader began as waffle-iron's (same
//! author), and the router's `board` crate builds its model from this one.

pub mod canon;
pub mod copper;
pub mod dru;
pub mod model;
pub mod outline;
pub mod place;
pub mod read;
pub mod rules;
pub mod sexpr;
pub mod text;
pub mod write;

pub use copper::{read_copper, read_copper_with, ArcTrack, Copper, CopperLayer, Group, Keepout, Outline, PadCopper, PadShape, Span, Track, Via, Zone};
pub use model::*;
pub use outline::{
    arc_is_ccw, chain, circumcenter, Loop, OutlineError, OutlineLoops, Segment, OUTLINE_WELD_M,
};
pub use read::MIN_VERSION;
pub use rules::{NetClass, Rules, RulesError};
pub use write::{apply, mm, Edit, NewArc, NewTrack, NewVia};

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

/// The board and the copper a router works with, from one parse, in an order of what they are
/// and not of the file's (see [`canon`]): a board reads the same however its file orders it.
pub fn parse_board(text: &str) -> Result<(Pcb, Copper), KicadParse> {
    parse_board_with(text, text::shaper())
}

/// [`parse_board`], its copper and mask text shaped by `shaper` rather than the environment's.
pub fn parse_board_with(text: &str, shaper: text::Shaper) -> Result<(Pcb, Copper), KicadParse> {
    let mut root = sexpr::parse(text)?;
    sexpr::number_nets(&mut root);
    let mut pcb = read::read(&root)?;
    let mut copper = copper::read_copper_with(&root, &pcb.footprints, shaper)?;
    // Last, as text is stroked by the file's footprint indices.
    canon::canonical(&mut pcb, &mut copper);
    Ok((pcb, copper))
}

impl Pcb {
    /// A net's number in the file, where [`parse_board`] numbered it by name: what the writer
    /// wants. A number it did not give stands for itself.
    pub fn file_net(&self, net: u32) -> u32 {
        self.file_nets.get(net as usize).copied().unwrap_or(net)
    }

    /// The Edge.Cuts primitives chained into the board loop and its
    /// cutouts.
    pub fn outline_loops(&self) -> Result<OutlineLoops, OutlineError> {
        outline::chain(&self.outline)
    }
}
