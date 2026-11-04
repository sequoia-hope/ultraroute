//! Text shaped by a helper program, here a stand-in script, and boxes when it is absent or fails.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use kicad_pcb::text::{Item, Shaper, HELPER_TIMEOUT};

/// One board text, and a footprint at (10, 10) turned 90 degrees with one text of its own.
const BOARD: &str = "(kicad_pcb (version 20240108) (general (thickness 1.6))
  (layers (0 \"F.Cu\" signal) (31 \"B.Cu\" signal) (39 \"F.Mask\" user)) (net 0 \"\")
  (gr_text \"AB\\nC\" (at 5 6 30) (layer \"F.Cu\") (effects (font (size 1.5 1.2) (thickness 0.2) bold (line_spacing 1.3)) (justify left top mirror)))
  (footprint \"R\" (layer \"F.Cu\") (at 10 10 90) (property \"Reference\" \"R1\" (at 0 0 90) (layer \"F.Mask\") (effects (font (size 1 1) (thickness 0.15))))))";

const ANSWERS: &str = "{\"polygons\":[[[0,0],[1,0],[1,1]]]}\n{\"polygons\":[[[1,0],[2,0],[2,1],[1,1]]]}\n";

/// The stand-in helpers, each saving what it is asked into `NAME.asked` if it reads it. All are
/// written once before any runs: a script being written while another test forks is `Text file
/// busy` to run.
fn helpers() -> &'static PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("text_helper");
        std::fs::create_dir_all(&dir).unwrap();
        let answer = |reply: &str, code: i32| format!("cat > \"$0.asked\"\nprintf '%s' '{reply}'\nexit {code}");
        let scripts = [
            ("answers", answer(ANSWERS, 0)),
            ("blank", answer("{\"polygons\":[]}\n{\"polygons\":[[[1,0],[2,0],[2,1]]]}\n", 0)),
            ("fails", answer("", 3)),
            ("short", answer("{\"polygons\":[]}\n", 0)),
            ("refuses", answer("{\"polygons\":[]}\n{\"error\":\"no\"}\n", 0)),
            ("hangs", "exec sleep 5".to_string()),
            // Leaves a process behind holding the pipes open after it is killed.
            ("leaves-a-child", "sleep 5\nexit 0".to_string()),
            ("answers-then-hangs", "cat > /dev/null\necho '{}'\nexec sleep 5 >&-".to_string()),
        ];
        for (name, script) in scripts {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    })
}

fn boxes() -> kicad_pcb::Copper {
    kicad_pcb::parse_board_with(BOARD, Shaper::Boxes("none here")).unwrap().1
}

#[test]
fn the_helper_is_asked_for_every_text_at_once_in_the_files_terms() {
    let path = helpers().join("answers");
    let (_, copper) = kicad_pcb::parse_board_with(BOARD, Shaper::Helper(&path, HELPER_TIMEOUT)).unwrap();
    let items: Vec<Item> = std::fs::read_to_string(helpers().join("answers.asked")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(items.len(), 2);
    let t = &items[0];
    assert_eq!((t.text.as_str(), t.size, t.thickness, t.at, t.angle), ("AB\nC", [1.2, 1.5], Some(0.2), [5.0, 6.0], 30.0));
    assert_eq!((t.h_align.as_str(), t.v_align.as_str(), t.mirror, t.bold, t.italic, t.knockout, t.line_spacing), ("left", "top", true, true, false, false, 1.3));
    // The footprint's text in the footprint's frame: its angle on the board less the footprint's.
    assert_eq!((items[1].text.as_str(), items[1].at, items[1].angle, items[1].h_align.as_str()), ("R1", [0.0, 0.0], 0.0, "center"));
    assert_eq!(copper.graphics[0].polygons, vec![vec![[0.0, 0.0], [1e-3, 0.0], [1e-3, 1e-3]]]);
    // Placed on the board as the footprint is: turned 90 degrees and moved to (10, 10).
    let placed = &copper.mask_graphics[0].polygons[0];
    let want = [[10.0, 9.0], [10.0, 8.0], [11.0, 8.0], [11.0, 9.0]];
    for (p, w) in placed.iter().zip(want) {
        assert!((p[0] - w[0] * 1e-3).abs() < 1e-12 && (p[1] - w[1] * 1e-3).abs() < 1e-12, "{placed:?}");
    }
    assert_eq!(copper.text_note, Some(format!("2 text item(s) stroked by {}", path.display())));
}

#[test]
fn a_text_the_helper_strokes_nothing_of_is_dropped() {
    let path = helpers().join("blank");
    let (_, copper) = kicad_pcb::parse_board_with(BOARD, Shaper::Helper(&path, HELPER_TIMEOUT)).unwrap();
    assert!(copper.graphics.is_empty() && copper.mask_graphics.len() == 1);
}

#[test]
fn without_a_helper_or_when_it_fails_text_is_boxes() {
    let boxed = boxes();
    assert_eq!(boxed.graphics[0].polygons.len(), 1, "one box");
    assert_eq!(boxed.text_note.as_deref(), Some("2 text item(s) as boxes: none here"));
    for (name, why) in [("fails", "exited with exit status: 3"), ("short", "1 replies to 2 texts"), ("refuses", "text 2: no")] {
        let path = helpers().join(name);
        let (_, copper) = kicad_pcb::parse_board_with(BOARD, Shaper::Helper(&path, HELPER_TIMEOUT)).unwrap();
        assert_eq!((&copper.graphics, &copper.mask_graphics), (&boxed.graphics, &boxed.mask_graphics), "{name}");
        let note = copper.text_note.unwrap();
        assert!(note.starts_with("2 text item(s) as boxes: the helper failed:") && note.ends_with(why), "{name}: {note}");
    }
    let missing = helpers().join("missing");
    let (_, copper) = kicad_pcb::parse_board_with(BOARD, Shaper::Helper(&missing, HELPER_TIMEOUT)).unwrap();
    assert_eq!(copper.graphics, boxed.graphics);
}

#[test]
fn a_helper_that_hangs_is_killed_and_text_is_boxes() {
    let boxed = boxes();
    for name in ["hangs", "leaves-a-child", "answers-then-hangs"] {
        let path = helpers().join(name);
        let started = std::time::Instant::now();
        let (_, copper) = kicad_pcb::parse_board_with(BOARD, Shaper::Helper(&path, std::time::Duration::from_millis(300))).unwrap();
        assert!(started.elapsed().as_secs() < 3, "{name} took {:?}", started.elapsed());
        assert_eq!((&copper.graphics, &copper.mask_graphics), (&boxed.graphics, &boxed.mask_graphics), "{name}");
        let note = copper.text_note.unwrap();
        assert!(note.starts_with("2 text item(s) as boxes: the helper failed:") && note.ends_with("did not finish within 0.3 s"), "{name}: {note}");
    }
}
