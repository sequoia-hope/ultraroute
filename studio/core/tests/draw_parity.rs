//! The mesh a recorded run draws, pinned by hash: the board, its routing frames, the router's
//! fills, and the client's views of them (masks, hidden layers, high contrast, a dragged part,
//! picks, locks, congestion and a part being tried). A change that alters any vertex changes a
//! hash, so moving the drawing elsewhere can show it draws the same bytes.
//!
//! The stream is easy2350 routed for 8 s by `ultraroute route ... --live`.

use studio_core::Scene;

const STREAM: &str = include_str!("fixtures/easy2350-stream.jsonl");

/// FNV-1a over the length and every float's bits.
fn hash(mesh: &[f32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in (mesh.len() as u64).to_le_bytes().into_iter().chain(mesh.iter().flat_map(|v| v.to_bits().to_le_bytes())) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn set_offset(s: &mut Scene, part: i32, dx: f32, dy: f32) {
    s.set_offset(part, dx, dy);
}

fn set_hidden(s: &mut Scene, layer: usize, hidden: bool) {
    s.board.set_layer_visible(layer, !hidden);
}

fn set_hot_net(s: &mut Scene, net: i32) {
    s.board.style.hot_net = Some(net);
}

fn set_active(s: &mut Scene, layer: usize, high_contrast: bool) {
    s.board.style.active_layer = layer;
    s.board.style.high_contrast = high_contrast;
}

fn set_pours(s: &mut Scene, on: bool) {
    s.board.style.show_pours = on;
}

/// Hashes of the mesh after each line of the stream, then of each view of the finished board.
fn hashes() -> Vec<u64> {
    let mut s = Scene::default();
    let px = 0.02;
    let mut out = Vec::new();
    set_pours(&mut s, true);
    for line in STREAM.lines() {
        s.apply(line);
        out.push(hash(&s.mesh_with(px, u64::MAX, 0.0)));
    }
    // Before the router's fills the file's pours draw; after them, only the fills.
    let views: Vec<Box<dyn Fn(&mut Scene)>> = vec![
        Box::new(|s| set_pours(s, false)),
        Box::new(|s| set_pours(s, true)),
        Box::new(|s| set_hidden(s, 1, true)),
        Box::new(|s| set_hidden(s, 1, false)),
        Box::new(|s| set_active(s, 3, true)),
        Box::new(|s| set_active(s, 2, false)),
        Box::new(|s| set_active(s, 0, false)),
        Box::new(|s| set_hot_net(s, 2)),
        Box::new(|s| set_offset(s, 1, 1.25, -0.5)),
        Box::new(|s| set_offset(s, 2, -3.0, 2.0)),
        Box::new(|s| s.picked = Some(1)),
        Box::new(|s| s.selected = Some(2)),
        Box::new(|s| s.highlight = vec![3, 4]),
        Box::new(|s| {
            s.lock_overrides.insert(3, true);
        }),
        Box::new(|s| {
            s.apply(r#"{"t":"frame","seq":99,"ms":9,"add":[],"del":[],"parts":[],"probes":[],"events":[],"focus":{"phase":"nudging","part":2,"ghost":[0.5,-0.25]}}"#);
        }),
        Box::new(|s| s.show_congestion = true),
    ];
    for view in &views {
        view(&mut s);
        for (mask, now) in [(u64::MAX, 0.0), (1, 100.0), (1 << 3, 700.0), (0b0110, 2000.0)] {
            out.push(hash(&s.mesh_with(px, mask, now)));
        }
        out.push(hash(&s.mesh_with(0.15, u64::MAX, 0.0)));
    }
    // Picking over a grid across the board, a quarter millimetre apart.
    let (mut parts, mut nets) = (Vec::new(), Vec::new());
    for i in 0..=84 {
        for j in 0..=204 {
            let (x, y) = (100.0 + i as f32 * 0.25, -151.0 + j as f32 * 0.25);
            parts.push(s.part_at(x, y).unwrap_or(0) as f32);
            nets.push(s.net_at(x, y, 0.3).unwrap_or(0) as f32);
        }
    }
    assert!(parts.iter().filter(|p| **p != 0.0).count() > 1000 && nets.iter().filter(|n| **n != 0.0).count() > 1000);
    out.push(hash(&parts));
    out.push(hash(&nets));
    out
}

#[test]
fn a_recorded_run_draws_the_same_bytes() {
    let got = hashes();
    if std::env::var_os("PRINT_HASHES").is_some() {
        println!("{got:#x?}");
    }
    assert_eq!(got, EXPECTED.to_vec());
}

const EXPECTED: [u64; 105] = [
    0xa8c7f832281a39c5, 0x7d293170cb606212, 0x879b4c0d5e39a164, 0xe294dff37f5854d5,
    0x5d726cff2c20e527, 0xa51bab2fa635aede, 0x308fad808f741ea8, 0xed51c81c76911b,
    0x448ced679bdd2eb9, 0x7fb4c3213cdbe678, 0x5c698b13842eb6fe, 0x1de3c26babc72c0c,
    0x3d818ba7c26af3e2, 0xb2095f1434e2a028, 0xbf08c5c105b2473f, 0xc6ca8d922c026a2c,
    0xcb54bfd8762b5307, 0x6ccc1f93849d6a85, 0x63489f43ce51d26d, 0xd6eadd44aac0e695,
    0xbbb13b5f4ae8ec4e, 0x8ceca8152da0a90, 0x7145951d7ac789d8, 0xf0e0c79a3639ea4e,
    0x579a041eead1de7e, 0x4a4e4b2863e700db, 0x357c2a620595a294, 0x4534f806bef9c961,
    0x7145951d7ac789d8, 0xee62589b6cfef90f, 0x4a4e4b2863e700db, 0x2f1b3a714963ad27,
    0xc6ad7d8f877ab943, 0xcdfa4f06c77dfe43, 0xee62589b6cfef90f, 0x4a4e4b2863e700db,
    0xe9c83f361f26e8b4, 0x6ebd9709ee75083d, 0x7145951d7ac789d8, 0xee62589b6cfef90f,
    0x4a4e4b2863e700db, 0x2f1b3a714963ad27, 0xc6ad7d8f877ab943, 0x52200c44390da914,
    0x1a8a1c034b1f02af, 0x4a4e4b2863e700db, 0x8589a3b0846859b7, 0x699ac0b9139d6177,
    0x5c0b9a2598b35614, 0xee62589b6cfef90f, 0x4a4e4b2863e700db, 0x53bf694fe384d653,
    0xb93f79f364f002e3, 0x7145951d7ac789d8, 0xee62589b6cfef90f, 0x4a4e4b2863e700db,
    0x2f1b3a714963ad27, 0xc6ad7d8f877ab943, 0x41a180a71a2893d8, 0xbed94ed2e6dc6a7,
    0xa9a70237c7d954b3, 0xb3b8ae6dcc29984f, 0x1e46d9e2d411724f, 0x746eaba216a64dbe,
    0xb9b76ce86762202d, 0xa9a70237c7d954b3, 0xb3b8ae6dcc29984f, 0x9fa33e3710c0f408,
    0xa8220aab3e8a47ca, 0x6a501a4a048009f1, 0xa9a70237c7d954b3, 0xb3b8ae6dcc29984f,
    0x14cd4225426b0b14, 0x94c7935cc7389cb6, 0x730cb3be638bc275, 0x370f94c99b0c7379,
    0x2df6ebdecbddbee5, 0x617e799e46ee1ba1, 0xc7867b9b0f652597, 0x209fc83b692688c7,
    0xde709205b25dab8d, 0xbd2a51f267a77b09, 0x21cdbd5118e1d334, 0x7b2547a1d2dcafce,
    0x57f68e6cfefb3666, 0x46f2951b3f489b04, 0xc65e89fc8303bfc0, 0xed4aa388de6b5f78,
    0x7b2547a1d2dcafce, 0x57f68e6cfefb3666, 0x46f2951b3f489b04, 0xc65e89fc8303bfc0,
    0xed4aa388de6b5f78, 0xf76f47b33d69883e, 0x915c0cb738563828, 0xd14acdbb11e61614,
    0xfc8e5077b883c725, 0xa773beece92bcf82, 0xc2fd1391b85ab9b0, 0x30a89b45a6b39643,
    0x5b6277f208624464, 0xfc8e5077b883c725, 0x51f45f67f4f7931a, 0x39416bd81d766aa3,
    0xef26ecb8b2c938c2,
];
