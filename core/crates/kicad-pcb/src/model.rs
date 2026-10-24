//! The neutral board record a `.kicad_pcb` reads into.
//!
//! Units and frame (specs/kicad_board_link.md §2.2): every length is in
//! **metres**, every angle in **degrees**. Positions are in the **board
//! frame as the file stores it: Y down**, so `y` grows toward the bottom of
//! the screen; the Y flip that puts the board into a right-handed sketch
//! frame belongs to the derive step (§2.3), not here. Footprint-local
//! pad and graphic positions have already been placed into the board frame
//! by the reader ([`Footprint::place`]).

use serde::{Deserialize, Serialize};

/// Whole-file record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pcb {
    /// The `(version N)` stamp (a date, `YYYYMMDD`).
    pub version: u32,
    /// `(generator …)` text, when present (`pcbnew`, `eeschema`…).
    pub generator: Option<String>,
    /// The thickness the board is extruded by: the stackup's copper +
    /// dielectric sum when a stackup is present, else `general.thickness`.
    pub thickness_m: f64,
    /// `(general (thickness T))`, when present.
    pub thickness_general_m: Option<f64>,
    /// Sum of `(setup (stackup (layer … (thickness t))))` over `copper`,
    /// `core` and `prepreg` layers, when a stackup with thicknesses exists.
    pub thickness_stackup_m: Option<f64>,
    pub title_block: TitleBlock,
    /// Number of `(layers …)` entries whose name ends in `.Cu`.
    pub copper_layers: u32,
    /// Top-level `(net N "NAME")` entries, net 0 (the unconnected net)
    /// included, in file order.
    pub nets: Vec<Net>,
    /// Every primitive on `Edge.Cuts`, board frame, metres, in file order:
    /// board-level `gr_*` first, then each footprint's `fp_*` placed.
    pub outline: Vec<OutlinePrimitive>,
    pub footprints: Vec<Footprint>,
    /// Never one line per skipped form: a count per category.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TitleBlock {
    pub title: String,
    pub date: String,
    pub rev: String,
    pub company: String,
    /// `(comment N "text")`, ordered by N.
    pub comments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
    pub number: u32,
    pub name: String,
}

/// One `Edge.Cuts` primitive in the board frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutlinePrimitive {
    pub shape: OutlineShape,
    /// The footprint (uuid) whose `fp_*` graphic this is; `None` for a
    /// board-level `gr_*`.
    pub footprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OutlineShape {
    Line {
        start: [f64; 2],
        end: [f64; 2],
    },
    /// KiCad 6+ stores an arc by three points; the direction of travel is
    /// the one that visits `mid` between `start` and `end`. The centre is
    /// the circumcentre of the three points (computed, never read from a
    /// cached `(center …)`).
    Arc {
        start: [f64; 2],
        mid: [f64; 2],
        end: [f64; 2],
    },
    Circle {
        center: [f64; 2],
        radius: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// `(layer "F.Cu")`
    Front,
    /// `(layer "B.Cu")`
    Back,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    /// `(uuid …)` (KiCad 8+) or `(tstamp …)` (KiCad 6/7).
    pub uuid: String,
    /// `"Library:Name"`.
    pub library_id: String,
    pub side: Side,
    /// `(at x y [rot])`: origin in the board frame, metres.
    pub at: [f64; 2],
    /// Rotation in degrees, counter-clockwise as KiCad draws it (in the
    /// Y-down frame: `x' = x cos + y sin`, `y' = y cos − x sin`).
    pub rotation_deg: f64,
    pub reference: String,
    pub value: String,
    /// The `Footprint` property (`"MountingHole:MountingHole_3.2mm"`), or
    /// `library_id` when the file has no such property (KiCad 6).
    pub footprint: String,
    pub datasheet: String,
    /// `(attr …)` tokens: `smd`, `through_hole`, `board_only`,
    /// `exclude_from_pos_files`, `exclude_from_bom`, `dnp`…
    pub attrs: Vec<String>,
    pub pads: Vec<Pad>,
    pub models: Vec<Model>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PadKind {
    ThruHole,
    Smd,
    /// Non-plated through hole — what a mounting hole usually is.
    NpThruHole,
    Connect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pad {
    pub number: String,
    pub kind: PadKind,
    /// `circle`, `oval`, `rect`, `roundrect`, `trapezoid`, `custom`.
    pub shape: String,
    /// Centre in the board frame, metres (footprint `at` applied).
    pub position: [f64; 2],
    /// Absolute rotation, degrees (footprint rotation + the pad's
    /// footprint-relative angle).
    pub rotation_deg: f64,
    /// `(size w h)`, metres.
    pub size: [f64; 2],
    /// `(drill d)` diameter (or `(drill oval w h)` ⇒ `[w, h]`), metres.
    pub drill: Option<[f64; 2]>,
    pub net: Option<Net>,
}

impl Pad {
    /// A hole that nothing connects to: a non-plated through hole, or a
    /// plated one with no net. The mounting-hole rule of the spec (§2.3)
    /// combines this with the footprint name.
    pub fn is_unconnected_hole(&self) -> bool {
        match self.kind {
            PadKind::NpThruHole => true,
            PadKind::ThruHole => self.net.as_ref().is_none_or(|n| n.name.is_empty()),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// As written, variables unexpanded (`${KICAD9_3DMODEL_DIR}/…`).
    pub path: String,
    /// `(offset (xyz …))`, metres (KiCad writes the offset in mm).
    pub offset_m: [f64; 3],
    pub scale: [f64; 3],
    /// `(rotate (xyz …))`, degrees, KiCad's sign convention (§2.3).
    pub rotate_deg: [f64; 3],
    /// `(hide yes)` / `hide` present.
    pub hidden: bool,
}

impl Footprint {
    /// Board-frame position of a footprint-local point: KiCad's
    /// `RotatePoint(local, orientation) + at` in the Y-down frame. The same
    /// rule serves both sides: the file already stores a back-side
    /// footprint's local coordinates mirrored.
    pub fn place(&self, local: [f64; 2]) -> [f64; 2] {
        place(self.at, self.rotation_deg, local)
    }

    /// Footprint-name test the spec's mounting-hole rule uses.
    pub fn is_mounting_hole_footprint(&self) -> bool {
        self.footprint.starts_with("MountingHole")
            || self
                .footprint
                .split(':')
                .nth(1)
                .is_some_and(|n| n.starts_with("MountingHole"))
    }
}

/// KiCad's `RotatePoint` (Y-down frame, positive = counter-clockwise on
/// screen) followed by the translation to `at`.
pub fn place(at: [f64; 2], rotation_deg: f64, local: [f64; 2]) -> [f64; 2] {
    let (s, c) = rotation_deg.to_radians().sin_cos();
    let [x, y] = local;
    [at[0] + x * c + y * s, at[1] + y * c - x * s]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_rotates_counter_clockwise_on_screen_in_y_down_frame() {
        // A point on +X, rotated +90°, ends up on −Y (up on screen).
        let p = place([0.0, 0.0], 90.0, [1.0, 0.0]);
        assert!((p[0]).abs() < 1e-15 && (p[1] + 1.0).abs() < 1e-15, "{p:?}");
        let q = place([10.0, 20.0], 0.0, [1.0, 2.0]);
        assert_eq!(q, [11.0, 22.0]);
    }

    #[test]
    fn unconnected_hole_rule() {
        let mut pad = Pad {
            number: "1".into(),
            kind: PadKind::ThruHole,
            shape: "circle".into(),
            position: [0.0, 0.0],
            rotation_deg: 0.0,
            size: [1e-3, 1e-3],
            drill: Some([5e-4, 5e-4]),
            net: Some(Net {
                number: 1,
                name: "GND".into(),
            }),
        };
        assert!(!pad.is_unconnected_hole());
        pad.net = None;
        assert!(pad.is_unconnected_hole());
        pad.kind = PadKind::Smd;
        assert!(!pad.is_unconnected_hole());
        pad.kind = PadKind::NpThruHole;
        pad.net = Some(Net {
            number: 1,
            name: "GND".into(),
        });
        assert!(pad.is_unconnected_hole());
    }

    #[test]
    fn mounting_hole_footprint_name_rule() {
        let mut fp = Footprint {
            uuid: "u".into(),
            library_id: "MountingHole:MountingHole_3.2mm".into(),
            side: Side::Front,
            at: [0.0, 0.0],
            rotation_deg: 0.0,
            reference: "H1".into(),
            value: "".into(),
            footprint: "MountingHole:MountingHole_3.2mm".into(),
            datasheet: "".into(),
            attrs: vec![],
            pads: vec![],
            models: vec![],
        };
        assert!(fp.is_mounting_hole_footprint());
        fp.footprint = "Resistor_SMD:R_0603".into();
        assert!(!fp.is_mounting_hole_footprint());
    }
}
