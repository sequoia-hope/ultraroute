//! `spoke_audit BARE.kicad_pcb [ROUTED.kicad_pcb]`: the routed board's tracks and vias laid onto
//! the bare board (or, with no routed board, the bare board's own), and those the checker
//! refuses for cutting a pad's thermal spokes.

use std::path::PathBuf;

use board::{check_item, Board, ItemId, Kind};

fn main() {
    let mut args = std::env::args().skip(1).map(PathBuf::from);
    let bare = args.next().expect("usage: spoke_audit BARE [ROUTED]");
    let (mut board, _) = Board::load(&bare).unwrap_or_else(|e| panic!("{e}"));
    let laid: Vec<ItemId> = match args.next() {
        Some(routed) => {
            let (out, _) = Board::load(&routed).unwrap_or_else(|e| panic!("{e}"));
            let copper: Vec<_> = out.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(_, it)| it.clone()).collect();
            copper.into_iter().map(|it| board.insert(it)).collect()
        }
        None => board.items().filter(|(_, it)| matches!(it.kind, Kind::Track | Kind::Via)).map(|(id, _)| id).collect(),
    };
    let mut hits = 0;
    for id in laid {
        let it = board.item(id).expect("laid").clone();
        for v in check_item(&board, id) {
            let Some(spoke) = v.b.and_then(|b| board.item(b)) else { continue };
            let Kind::Spoke { footprint, pad, need, spare, .. } = spoke.kind else { continue };
            hits += 1;
            let number = match &board.item(pad).map(|p| &p.kind) {
                Some(Kind::Pad { number, .. }) => number.clone(),
                _ => "?".into(),
            };
            println!(
                "{} {:?} on {}: {:.3} mm from {}-{number}'s spoke ({}; needs {need}, {spare} spare), {:.3} required",
                board.net_name(it.net),
                it.shape,
                board.layers[v.layer.unwrap_or(0)],
                v.gap * 1e-6,
                board.footprints[footprint].reference,
                board.net_name(spoke.net),
                v.required as f64 * 1e-6
            );
        }
    }
    println!("{hits} tracks and vias cut into a spoke too many");
}
