//! Copper and solder mask text as KiCad strokes it, behind a process boundary.
//!
//! `kicad-text` reads one JSON [`Item`] a line on standard input and writes one JSON [`Reply`] a
//! line on standard output, in the same order: the polygons KiCad 9's DRC sees for the text,
//! millimetres, Y down, in the frame `at` is given in. The MIT reader (`kicad-pcb`'s `text`
//! module) speaks this and falls back to a box round each text when the program is absent.
//!
//! GPL-3.0-or-later, as the font table and layout it carries come from KiCad.

pub mod font;

use serde::{Deserialize, Serialize};

/// One text item as the file gives it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub text: String,
    /// Glyph width and height, mm (the file's `(size H W)` is height first).
    pub size: [f64; 2],
    /// `(thickness T)`, mm; none or 0 for KiCad's default for the size.
    #[serde(default)]
    pub thickness: Option<f64>,
    /// The anchor, mm.
    pub at: [f64; 2],
    /// Degrees, counter-clockwise on screen.
    #[serde(default)]
    pub angle: f64,
    /// `left`, `center` or `right`.
    #[serde(default = "center")]
    pub h_align: String,
    /// `top`, `center` or `bottom`.
    #[serde(default = "center")]
    pub v_align: String,
    #[serde(default)]
    pub mirror: bool,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub knockout: bool,
    #[serde(default = "one")]
    pub line_spacing: f64,
}

fn center() -> String {
    "center".to_string()
}

fn one() -> f64 {
    1.0
}

/// The answer to one [`Item`]: its polygons, mm, or why there are none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply {
    Polygons { polygons: Vec<Vec<[f64; 2]>> },
    Error { error: String },
}

const MM: f64 = 1e-3;

/// The polygons of `item`, mm, to the nanometre.
pub fn shapes(item: &Item) -> Result<Vec<Vec<[f64; 2]>>, String> {
    let h_align = match item.h_align.as_str() {
        "left" => font::HAlign::Left,
        "center" => font::HAlign::Center,
        "right" => font::HAlign::Right,
        other => return Err(format!("h_align {other:?}: left, center or right")),
    };
    let v_align = match item.v_align.as_str() {
        "top" => font::VAlign::Top,
        "center" => font::VAlign::Center,
        "bottom" => font::VAlign::Bottom,
        other => return Err(format!("v_align {other:?}: top, center or bottom")),
    };
    let style = font::TextStyle {
        width: item.size[0] * MM,
        height: item.size[1] * MM,
        thickness: item.thickness.filter(|&t| t > 0.0).map(|t| t * MM),
        bold: item.bold,
        italic: item.italic,
        h_align,
        v_align,
        mirror: item.mirror,
        knockout: item.knockout,
        line_spacing: item.line_spacing,
    };
    let at = [item.at[0] * MM, item.at[1] * MM];
    Ok(font::polygons(&item.text, at, item.angle, &style).into_iter().map(|p| p.into_iter().map(|[x, y]| [nanometre(x / MM), nanometre(y / MM)]).collect()).collect())
}

fn nanometre(mm: f64) -> f64 {
    (mm * 1e6).round() / 1e6
}

/// One reply line for one request line.
pub fn answer(line: &str) -> Reply {
    match serde_json::from_str::<Item>(line).map_err(|e| e.to_string()).and_then(|item| shapes(&item)) {
        Ok(polygons) => Reply::Polygons { polygons },
        Err(error) => Reply::Error { error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polygons(line: &str) -> Vec<Vec<[f64; 2]>> {
        match answer(line) {
            Reply::Polygons { polygons } => polygons,
            Reply::Error { error } => panic!("{error}"),
        }
    }

    #[test]
    fn an_item_needs_only_its_text_size_and_anchor() {
        let one = polygons("{\"text\":\"I\",\"size\":[1,1],\"at\":[10,20]}");
        assert_eq!(one.len(), 1, "one stroke, one capsule");
        assert!(one[0].iter().all(|p| (p[0] - 10.0).abs() < 0.2), "centred on the anchor: {:?}", one[0]);
    }

    #[test]
    fn line_spacing_spreads_the_lines() {
        let tall = |spacing: f64| {
            let ys: Vec<f64> = polygons(&format!("{{\"text\":\"I\\nI\",\"size\":[1,1],\"at\":[0,0],\"line_spacing\":{spacing}}}")).iter().flatten().map(|p| p[1]).collect();
            ys.iter().copied().fold(f64::MIN, f64::max) - ys.iter().copied().fold(f64::MAX, f64::min)
        };
        assert!(tall(2.0) > tall(1.0) + 1.0, "{} vs {}", tall(2.0), tall(1.0));
    }

    #[test]
    fn a_bad_item_is_an_error_reply() {
        assert!(matches!(answer("{\"text\":\"I\",\"size\":[1,1],\"at\":[0,0],\"h_align\":\"up\"}"), Reply::Error { .. }));
        assert!(matches!(answer("not json"), Reply::Error { .. }));
    }
}
