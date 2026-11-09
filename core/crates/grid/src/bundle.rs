//! Squaring parallel slanted tracks together, as a bus.
//!
//! Single-chain squaring bends one net at a time. Where several nets run side by side at the same
//! odd angle near minimum spacing, no single track can take a knee or a Z on its own: its knee
//! cuts across the neighbour's slanted line, so every member refuses and stays slanted (or outer
//! tracks peel off in mismatched Zs that break the bundle). Removing all members' slanted runs at
//! once and laying coordinated knees or staggered Zs in the same bend direction keeps their
//! edge-to-edge gap across the bend equal to the gap along the run, checked all or none with the
//! same rules, joins, grazes, slots, pad-rim slants, doglegs and net connectivity as `try_way`.

use std::collections::{HashMap, HashSet};

use board::{check_item, joined, Board, Item, ItemId, Kind, Nm, Point, Shape};

use crate::cleanup::{
    chains, doglegs, grazes, leaves_slot, pad_centre, same_heading, square_trace, Chain,
    TightWatch, OCTILINEAR_SPAN, TAUT_MARGIN,
};
use crate::clock::Deadline;

/// Spatial hash cell size for finding nearby parallel off-45 segments, in nm (2 mm).
const CELL: i64 = 2_000_000;

/// Maximum angle between two segments in a group (1 degree, matching `tools/tidy.py`).
const PARALLEL_DEG: f64 = 1.0;

/// Maximum edge-to-edge gap between neighbouring members, in multiples of pair clearance.
const BESIDE_CLEARANCES: f64 = 3.0;

/// Minimum clearance floor when scaling `BESIDE_CLEARANCES`, in nm (0.1 mm, matching `tidy.py`).
const MIN_GAP_FLOOR: Nm = 100_000;

/// Maximum rounds of bundle squaring in one `square_bundles` call.
const MAX_BUNDLE_ROUNDS: usize = 2;

/// Hard cap on candidate group board trials in one `square_bundles` call, bounding runtime even
/// when the stage runs with an open `Deadline`.
const MAX_GROUP_TRIES: usize = 512;

/// Maximum length of a short transition kink absorbed when expanding a member's slanted span.
const SHORT_KINK: f64 = 500_000.0;

#[derive(Clone)]
struct OffSeg {
    chain_idx: usize,
    track_idx: usize,
    layer: u64,
    net: u32,
    a: Point,
    b: Point,
    half_width: Nm,
    len: f64,
    ux: f64,
    uy: f64,
}

#[derive(Clone, Debug)]
struct Member {
    chain_idx: usize,
    net: u32,
    core: (usize, usize),
    expanded: (usize, usize),
    across: f64,
}

#[derive(Clone)]
struct MemberPlan {
    chain_idx: usize,
    span: (usize, usize),
    path: Vec<Point>,
    pads: [Option<ItemId>; 2],
}

struct RawMember {
    member: Member,
    mid: (f64, f64),
    proj: (f64, f64),
    half_width: Nm,
}

type TakenTrack = (ItemId, Item);
type CommittedMember = (usize, u32, Vec<ItemId>, Vec<TakenTrack>);

/// Whether `a` to `b` runs more than 0.5 degrees off every multiple of 45 degrees (`tidy.py`).
fn is_off45(a: Point, b: Point) -> bool {
    if a == b {
        return false;
    }
    let deg = ((b.y - a.y) as f64).atan2((b.x - a.x) as f64).to_degrees().rem_euclid(45.0);
    deg.min(45.0 - deg) > 0.5
}

/// Whether every segment along `path` lies strictly on a multiple of 45 degrees.
fn all_octilinear(path: &[Point]) -> bool {
    path.windows(2).all(|w| {
        let (dx, dy) = (w[1].x - w[0].x, w[1].y - w[0].y);
        (dx != 0 || dy != 0) && (dx == 0 || dy == 0 || dx.abs() == dy.abs())
    })
}

/// Squares parallel slanted groups in `soft` together, updating `soft` and calling `watch` per
/// bent member net. Returns how many member runs were bent onto 45-degree multiples.
pub fn square_bundles(board: &mut Board, soft: &mut Vec<ItemId>, deadline: &Deadline, watch: &mut TightWatch) -> usize {
    let mut total_bent = 0;
    let mut tries = 0usize;
    for _ in 0..MAX_BUNDLE_ROUNDS {
        if deadline.expired() || tries >= MAX_GROUP_TRIES {
            break;
        }
        let soft_set: HashSet<ItemId> = soft.iter().copied().filter(|&id| board.item(id).is_some()).collect();
        if soft_set.len() < 2 {
            break;
        }
        let all_chains = chains(board, &soft_set);
        let groups = find_groups(board, &all_chains);
        if groups.is_empty() {
            break;
        }
        let mut used_chains: HashSet<usize> = HashSet::new();
        let mut replaced: HashSet<ItemId> = HashSet::new();
        let mut added: Vec<ItemId> = Vec::new();
        let mut round_bent = 0;
        for group in groups {
            if deadline.expired() || tries >= MAX_GROUP_TRIES {
                break;
            }
            if group.iter().any(|m| used_chains.contains(&m.chain_idx)) {
                continue;
            }
            if let Some(committed) = square_one_group(board, &all_chains, &group, deadline, &mut tries) {
                for (chain_idx, net, pieces, taken) in committed {
                    used_chains.insert(chain_idx);
                    for (old_id, _) in &taken {
                        replaced.insert(*old_id);
                    }
                    added.extend(&pieces);
                    let old_items: Vec<Item> = taken.into_iter().map(|(_, it)| it).collect();
                    watch(board, net, &old_items, &pieces);
                    round_bent += 1;
                }
            }
        }
        if round_bent == 0 {
            break;
        }
        soft.retain(|id| !replaced.contains(id) && board.item(*id).is_some());
        soft.extend(added);
        total_bent += round_bent;
    }
    total_bent
}

/// Finds side-by-side groups of 2+ off-45 segments of different nets on the same layer, parallel
/// within 1 degree, overlapping along their run, and spaced at most `3 * clearance` edge to edge.
fn find_groups(board: &Board, all_chains: &[Chain]) -> Vec<Vec<Member>> {
    let mut segs: Vec<OffSeg> = Vec::new();
    for (chain_idx, c) in all_chains.iter().enumerate() {
        if c.net == 0 {
            continue;
        }
        for k in 0..c.tracks.len() {
            let (a, b) = (c.points[k], c.points[k + 1]);
            if !is_off45(a, b) {
                continue;
            }
            let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
            let len = dx.hypot(dy);
            if len <= 0.0 {
                continue;
            }
            segs.push(OffSeg {
                chain_idx,
                track_idx: k,
                layer: c.layer,
                net: c.net,
                a,
                b,
                half_width: c.half_width,
                len,
                ux: dx / len,
                uy: dy / len,
            });
        }
    }
    let n = segs.len();
    if n < 2 {
        return Vec::new();
    }

    let mut grid: HashMap<(u64, i64, i64), Vec<usize>> = HashMap::new();
    for (idx, s) in segs.iter().enumerate() {
        let (x0, x1) = (s.a.x.min(s.b.x).div_euclid(CELL) - 1, s.a.x.max(s.b.x).div_euclid(CELL) + 1);
        let (y0, y1) = (s.a.y.min(s.b.y).div_euclid(CELL) - 1, s.a.y.max(s.b.y).div_euclid(CELL) + 1);
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                grid.entry((s.layer, cx, cy)).or_default().push(idx);
            }
        }
    }

    let sin_tol = PARALLEL_DEG.to_radians().sin();
    let mut parent: Vec<usize> = (0..n).collect();
    let find = |parent: &mut [usize], mut x: usize| -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    };

    let mut any_pair = false;
    for i in 0..n {
        let si = &segs[i];
        let mut near: Vec<usize> = Vec::new();
        let (x0, x1) = (si.a.x.min(si.b.x).div_euclid(CELL), si.a.x.max(si.b.x).div_euclid(CELL));
        let (y0, y1) = (si.a.y.min(si.b.y).div_euclid(CELL), si.a.y.max(si.b.y).div_euclid(CELL));
        for cx in x0..=x1 {
            for cy in y0..=y1 {
                if let Some(bucket) = grid.get(&(si.layer, cx, cy)) {
                    near.extend(bucket.iter().copied().filter(|&j| j > i));
                }
            }
        }
        near.sort_unstable();
        near.dedup();
        for j in near {
            let sj = &segs[j];
            if si.net == sj.net {
                continue;
            }
            if (si.ux * sj.uy - si.uy * sj.ux).abs() > sin_tol {
                continue;
            }
            let p0 = si.ux * (sj.a.x - si.a.x) as f64 + si.uy * (sj.a.y - si.a.y) as f64;
            let p1 = si.ux * (sj.b.x - si.a.x) as f64 + si.uy * (sj.b.y - si.a.y) as f64;
            let (lo, hi) = (0.0_f64.max(p0.min(p1)), si.len.min(p0.max(p1)));
            if hi <= lo {
                continue;
            }
            let q0 = sj.ux * (si.a.x - sj.a.x) as f64 + sj.uy * (si.a.y - sj.a.y) as f64;
            let q1 = sj.ux * (si.b.x - sj.a.x) as f64 + sj.uy * (si.b.y - sj.a.y) as f64;
            if sj.len.min(q0.max(q1)) <= 0.0_f64.max(q0.min(q1)) {
                continue;
            }
            let mid = 0.5 * (lo + hi);
            let (mx, my) = (si.a.x as f64 + mid * si.ux, si.a.y as f64 + mid * si.uy);
            let side = (sj.ux * (my - sj.a.y as f64) - sj.uy * (mx - sj.a.x as f64)).abs();
            let edge_gap = side - (si.half_width + sj.half_width) as f64;
            let clr = board.clearance(si.net, sj.net).max(MIN_GAP_FLOOR) as f64;
            if edge_gap <= BESIDE_CLEARANCES * clr {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
                any_pair = true;
            }
        }
    }
    if !any_pair {
        return Vec::new();
    }

    let mut comps: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        comps.entry(r).or_default().push(i);
    }

    let mut keys: Vec<usize> = comps.keys().copied().collect();
    keys.sort_unstable();
    let mut out: Vec<Vec<Member>> = Vec::new();
    for k in keys {
        let indices = &comps[&k];
        if indices.len() < 2 {
            continue;
        }
        build_groups_from_component(board, all_chains, &segs, indices, &mut out);
    }
    // Try larger and longer groups first.
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].chain_idx.cmp(&b[0].chain_idx)));
    out
}

/// Converts a union-find component of parallel segments into one or more transversely ordered
/// side-by-side bundles with at most one chain per net.
fn build_groups_from_component(
    board: &Board,
    all_chains: &[Chain],
    segs: &[OffSeg],
    indices: &[usize],
    out: &mut Vec<Vec<Member>>,
) {
    // Reference heading from the longest segment in the component.
    let best_ref = *indices.iter().max_by(|&&a, &&b| segs[a].len.total_cmp(&segs[b].len)).expect("len >= 2");
    let (rux, ruy) = (segs[best_ref].ux, segs[best_ref].uy);

    // Group segments by net, picking the chain with the largest parallel length on that net.
    let mut by_net: HashMap<u32, Vec<usize>> = HashMap::new();
    for &idx in indices {
        by_net.entry(segs[idx].net).or_default().push(idx);
    }
    if by_net.len() < 2 {
        return;
    }

    let mut nets: Vec<u32> = by_net.keys().copied().collect();
    nets.sort_unstable();

    let sin_tol = PARALLEL_DEG.to_radians().sin();
    let mut raw: Vec<RawMember> = Vec::new();
    let (mut sum_ux, mut sum_uy) = (0.0, 0.0);

    for net in nets {
        let net_segs = &by_net[&net];
        let mut by_chain: HashMap<usize, Vec<usize>> = HashMap::new();
        for &idx in net_segs {
            by_chain.entry(segs[idx].chain_idx).or_default().push(idx);
        }
        let (&chain_idx, chain_segs) = by_chain
            .iter()
            .max_by(|(ca, va), (cb, vb)| {
                let la: f64 = va.iter().map(|&i| segs[i].len).sum();
                let lb: f64 = vb.iter().map(|&i| segs[i].len).sum();
                la.total_cmp(&lb).then_with(|| cb.cmp(ca))
            })
            .expect("non-empty");
        let longest = *chain_segs.iter().max_by(|&&a, &&b| segs[a].len.total_cmp(&segs[b].len)).expect("non-empty");
        let s = &segs[longest];
        if (s.ux * ruy - s.uy * rux).abs() > sin_tol {
            continue;
        }
        let sgn = if s.ux * rux + s.uy * ruy >= 0.0 { 1.0 } else { -1.0 };
        sum_ux += sgn * s.ux * s.len;
        sum_uy += sgn * s.uy * s.len;

        let mut i0 = chain_segs.iter().map(|&i| segs[i].track_idx).min().unwrap();
        let mut j0 = chain_segs.iter().map(|&i| segs[i].track_idx + 1).max().unwrap();
        if j0 - i0 > OCTILINEAR_SPAN {
            i0 = s.track_idx;
            j0 = s.track_idx + 1;
        }
        let chain = &all_chains[chain_idx];
        let expanded = expand_span(chain, i0, j0);
        let mid = ((s.a.x + s.b.x) as f64 * 0.5, (s.a.y + s.b.y) as f64 * 0.5);
        let p0 = rux * s.a.x as f64 + ruy * s.a.y as f64;
        let p1 = rux * s.b.x as f64 + ruy * s.b.y as f64;
        raw.push(RawMember {
            member: Member { chain_idx, net, core: (i0, j0), expanded, across: 0.0 },
            mid,
            proj: (p0.min(p1), p0.max(p1)),
            half_width: s.half_width,
        });
    }
    if raw.len() < 2 {
        return;
    }

    let norm = sum_ux.hypot(sum_uy);
    if norm <= 0.0 {
        return;
    }
    let (ux, uy) = (sum_ux / norm, sum_uy / norm);
    for r in &mut raw {
        r.member.across = -uy * r.mid.0 + ux * r.mid.1;
    }
    raw.sort_by(|a, b| a.member.across.total_cmp(&b.member.across));

    // Split if two adjacent transverse lanes do not overlap or sit farther apart than 3x clearance.
    let mut start = 0;
    for k in 0..raw.len() {
        let split_after = if k + 1 < raw.len() {
            let (lo_a, hi_a) = raw[k].proj;
            let (lo_b, hi_b) = raw[k + 1].proj;
            let overlaps = hi_a.min(hi_b) > lo_a.max(lo_b);
            let clr = board.clearance(raw[k].member.net, raw[k + 1].member.net).max(MIN_GAP_FLOOR) as f64;
            let edge_gap = (raw[k + 1].member.across - raw[k].member.across) - (raw[k].half_width + raw[k + 1].half_width) as f64;
            !overlaps || edge_gap > BESIDE_CLEARANCES * clr
        } else {
            true
        };
        if split_after {
            if k + 1 - start >= 2 {
                out.push(raw[start..=k].iter().map(|x| x.member.clone()).collect());
            }
            start = k + 1;
        }
    }
}

/// Expands `(i0, j0)` along `chain` (up to `OCTILINEAR_SPAN` tracks) to include contiguous off-45
/// segments and tiny `< 0.5 mm` transition kinks heading the same general way, so multi-segment
/// taut bends into pads or diagonals square as one clean span.
fn expand_span(chain: &Chain, mut i: usize, mut j: usize) -> (usize, usize) {
    let (p0, p1) = (chain.points[i], chain.points[j]);
    let (dx, dy) = ((p1.x - p0.x) as f64, (p1.y - p0.y) as f64);
    let base_len = dx.hypot(dy);
    if base_len <= 0.0 {
        return (i, j);
    }
    let (ux, uy) = (dx / base_len, dy / base_len);
    let cos_max = 40.0_f64.to_radians().cos();

    let step_ok = |a: Point, b: Point| -> (bool, f64) {
        let (sx, sy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
        let sl = sx.hypot(sy);
        (sl > 0.0 && (sx * ux + sy * uy) >= cos_max * sl, sl)
    };

    loop {
        let mut grew = false;
        if i > 0 && j - i < OCTILINEAR_SPAN {
            let (ok, sl) = step_ok(chain.points[i - 1], chain.points[i]);
            let absorb = ok
                && (is_off45(chain.points[i - 1], chain.points[i])
                    || (sl <= SHORT_KINK
                        && i >= 2
                        && j - (i - 2) <= OCTILINEAR_SPAN
                        && step_ok(chain.points[i - 2], chain.points[i - 1]).0
                        && is_off45(chain.points[i - 2], chain.points[i - 1])));
            if absorb {
                i -= 1;
                grew = true;
            }
        }
        if j < chain.tracks.len() && j - i < OCTILINEAR_SPAN {
            let (ok, sl) = step_ok(chain.points[j], chain.points[j + 1]);
            let absorb = ok
                && (is_off45(chain.points[j], chain.points[j + 1])
                    || (sl <= SHORT_KINK
                        && j + 2 <= chain.tracks.len()
                        && (j + 2) - i <= OCTILINEAR_SPAN
                        && step_ok(chain.points[j + 1], chain.points[j + 2]).0
                        && is_off45(chain.points[j + 1], chain.points[j + 2])));
            if absorb {
                j += 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    (i, j)
}

/// Tries to bend `group` together (or, if a 3+ member group is blocked on one outer lane, its
/// contiguous sub-slices of size >= 2).
fn square_one_group(
    board: &mut Board,
    all_chains: &[Chain],
    group: &[Member],
    deadline: &Deadline,
    tries: &mut usize,
) -> Option<Vec<CommittedMember>> {
    if let Some(res) = try_member_slice(board, all_chains, group, deadline, tries) {
        return Some(res);
    }
    if group.len() >= 3 {
        for sub in [&group[..group.len() - 1], &group[1..]] {
            if deadline.expired() || *tries >= MAX_GROUP_TRIES {
                break;
            }
            if let Some(res) = try_member_slice(board, all_chains, sub, deadline, tries) {
                return Some(res);
            }
        }
    }
    None
}

fn try_member_slice(
    board: &mut Board,
    all_chains: &[Chain],
    members: &[Member],
    deadline: &Deadline,
    tries: &mut usize,
) -> Option<Vec<CommittedMember>> {
    let mut span_sets: Vec<Vec<(usize, usize)>> = vec![members.iter().map(|m| m.expanded).collect()];
    let core_spans: Vec<(usize, usize)> = members.iter().map(|m| m.core).collect();
    if span_sets[0] != core_spans {
        span_sets.push(core_spans);
    }

    for spans in span_sets {
        let candidates = group_ways(board, all_chains, members, &spans);
        for plans in candidates {
            if deadline.expired() || *tries >= MAX_GROUP_TRIES {
                return None;
            }
            *tries += 1;
            if try_bundle(board, all_chains, &plans, false).is_none() {
                continue;
            }
            // Check strict per-net connectivity before and after committing at exact width.
            let open_before: Vec<usize> = plans
                .iter()
                .map(|p| board::net_connectivity(board, all_chains[p.chain_idx].net, true).airlines.len())
                .collect();
            *tries += 1;
            let Some(committed) = try_bundle(board, all_chains, &plans, true) else { continue };
            let still_closed = plans.iter().zip(&open_before).all(|(p, &before)| {
                board::net_connectivity(board, all_chains[p.chain_idx].net, true).airlines.len() <= before
            });
            if still_closed {
                return Some(
                    plans
                        .iter()
                        .zip(committed)
                        .map(|(p, (pieces, taken))| (p.chain_idx, all_chains[p.chain_idx].net, pieces, taken))
                        .collect(),
                );
            }
            for (pieces, taken) in committed.into_iter().rev() {
                for id in pieces {
                    board.discard(id);
                }
                for (id, item) in taken {
                    board.restore(id, item);
                }
            }
        }
    }
    None
}

/// Generates coordinated octilinear ways for `members` over `spans`:
/// - 2-piece coordinated knees (axis-first for all, or diagonal-first for all), and
/// - 3-piece staggered-knee buses (axis-diagonal-axis and diagonal-axis-diagonal) where knee
///   positions are staggered so the edge-to-edge gap across the bend equals the gap along the run.
fn group_ways(
    board: &Board,
    all_chains: &[Chain],
    members: &[Member],
    spans: &[(usize, usize)],
) -> Vec<Vec<MemberPlan>> {
    let m = members.len();
    if m < 2 {
        return Vec::new();
    }

    // Reference forward direction along the bundle from member 0's span.
    let c0 = &all_chains[members[0].chain_idx];
    let (p0, q0) = (c0.points[spans[0].0], c0.points[spans[0].1]);
    let (ref_dx, ref_dy) = ((q0.x - p0.x) as f64, (q0.y - p0.y) as f64);
    if ref_dx == 0.0 && ref_dy == 0.0 {
        return Vec::new();
    }

    // Build up to two endpoint sets: exact chain points, and pad-centred ends when an end sits
    // alone in a pad (so tracks enter rectangular or round pads square through their centres).
    let mut raw_ends = Vec::with_capacity(m);
    let mut centred_ends = Vec::with_capacity(m);
    let mut any_centred = false;

    for (k, mem) in members.iter().enumerate() {
        let ch = &all_chains[mem.chain_idx];
        let (i, j) = spans[k];
        let (pa, pb) = (ch.points[i], ch.points[j]);
        let rev = ((pb.x - pa.x) as f64 * ref_dx + (pb.y - pa.y) as f64 * ref_dy) < 0.0;
        let (s, e) = if rev { (pb, pa) } else { (pa, pb) };
        raw_ends.push((s, e, rev, [None, None]));

        let pad_i = if i == 0 { pad_centre(board, ch, ch.tracks[0], pa) } else { None };
        let pad_j = if j == ch.tracks.len() { pad_centre(board, ch, ch.tracks[j - 1], pb) } else { None };
        if pad_i.is_some() || pad_j.is_some() {
            any_centred = true;
        }
        let pa_c = pad_i.map_or(pa, |(c, _)| c);
        let pb_c = pad_j.map_or(pb, |(c, _)| c);
        let (sc, ec, pads) = if rev {
            (pb_c, pa_c, [pad_j.map(|(_, id)| id), pad_i.map(|(_, id)| id)])
        } else {
            (pa_c, pb_c, [pad_i.map(|(_, id)| id), pad_j.map(|(_, id)| id)])
        };
        centred_ends.push((sc, ec, rev, pads));
    }

    let mut end_sets = vec![raw_ends];
    if any_centred {
        end_sets.push(centred_ends);
    }

    let mut out: Vec<Vec<MemberPlan>> = Vec::new();
    for ends in end_sets {
        build_ways_for_ends(board, all_chains, members, spans, &ends, &mut out);
    }
    out
}

#[allow(clippy::type_complexity)]
fn build_ways_for_ends(
    board: &Board,
    all_chains: &[Chain],
    members: &[Member],
    spans: &[(usize, usize)],
    ends: &[(Point, Point, bool, [Option<ItemId>; 2])],
    out: &mut Vec<Vec<MemberPlan>>,
) {
    let m = members.len();
    // Require at least one off-45 chord, and all chords in the same strict octant.
    let (s0, e0, _, _) = ends[0];
    let (dx0, dy0) = (e0.x - s0.x, e0.y - s0.y);
    if dx0 == 0 || dy0 == 0 || dx0.abs() == dy0.abs() {
        return;
    }
    let (sx, sy, horiz) = (dx0.signum(), dy0.signum(), dx0.abs() > dy0.abs());
    let to_mn = |p: Point| -> (Nm, Nm) {
        if horiz {
            (sx * p.x, sy * p.y)
        } else {
            (sy * p.y, sx * p.x)
        }
    };
    let from_mn = |maj: Nm, min: Nm| -> Point {
        if horiz {
            Point::new(sx * maj, sy * min)
        } else {
            Point::new(sx * min, sy * maj)
        }
    };

    let mut ms = Vec::with_capacity(m);
    let mut ns = Vec::with_capacity(m);
    let mut me = Vec::with_capacity(m);
    let mut ne = Vec::with_capacity(m);
    let (mut sum_dm, mut sum_dn) = (0.0, 0.0);

    for &(s, e, _, _) in ends {
        let (ms_k, ns_k) = to_mn(s);
        let (me_k, ne_k) = to_mn(e);
        let (dm, dn) = (me_k - ms_k, ne_k - ns_k);
        if dm <= dn || dn <= 0 {
            return;
        }
        ms.push(ms_k);
        ns.push(ns_k);
        me.push(me_k);
        ne.push(ne_k);
        sum_dm += dm as f64;
        sum_dn += dn as f64;
    }

    let len_u = sum_dm.hypot(sum_dn);
    if len_u <= 0.0 {
        return;
    }
    let (um, un) = (sum_dm / len_u, sum_dn / len_u);

    // Order lanes across the bundle in (M, N) space; reject if endpoints crossed lanes.
    let mut order: Vec<(usize, f64)> = (0..m)
        .map(|k| {
            let mid_m = 0.5 * (ms[k] + me[k]) as f64;
            let mid_n = 0.5 * (ns[k] + ne[k]) as f64;
            (k, -un * mid_m + um * mid_n)
        })
        .collect();
    order.sort_by(|a, b| a.1.total_cmp(&b.1));
    let idx: Vec<usize> = order.iter().map(|&(k, _)| k).collect();

    // Per-adjacent-lane minimum legal centre-to-centre gap and run centre-to-centre gap.
    let mut d_min = Vec::with_capacity(m - 1);
    let mut d_run = Vec::with_capacity(m - 1);
    for pos in 0..m - 1 {
        let (ka, kb) = (idx[pos], idx[pos + 1]);
        let (ca, cb) = (&all_chains[members[ka].chain_idx], &all_chains[members[kb].chain_idx]);
        let clr = board.clearance(ca.net, cb.net);
        let min_cc = (ca.half_width + cb.half_width + clr + 2 * TAUT_MARGIN + 500) as f64;
        let run_cc = (order[pos + 1].1 - order[pos].1).max(min_cc);
        d_min.push(min_cc);
        d_run.push(run_cc);
    }

    let make_plans = |pts_per_pos: Vec<Vec<Point>>| -> Option<Vec<MemberPlan>> {
        let mut by_member: Vec<Option<MemberPlan>> = vec![None; m];
        for (pos, mut pts) in pts_per_pos.into_iter().enumerate() {
            let k = idx[pos];
            pts.dedup();
            if pts.len() < 2 || !all_octilinear(&pts) {
                return None;
            }
            let (_, _, rev, mut pads) = ends[k];
            if rev {
                pts.reverse();
                pads.swap(0, 1);
            }
            by_member[k] = Some(MemberPlan { chain_idx: members[k].chain_idx, span: spans[k], path: pts, pads });
        }
        by_member.into_iter().collect()
    };

    // Candidate 2-piece knees: axis-first and diagonal-first.
    let sqrt2 = std::f64::consts::SQRT_2;
    let axis_first_pts: Vec<Vec<Point>> = (0..m)
        .map(|pos| {
            let k = idx[pos];
            let dn = ne[k] - ns[k];
            vec![from_mn(ms[k], ns[k]), from_mn(me[k] - dn, ns[k]), from_mn(me[k], ne[k])]
        })
        .collect();
    let diag_first_pts: Vec<Vec<Point>> = (0..m)
        .map(|pos| {
            let k = idx[pos];
            let dn = ne[k] - ns[k];
            vec![from_mn(ms[k], ns[k]), from_mn(ms[k] + dn, ne[k]), from_mn(me[k], ne[k])]
        })
        .collect();

    // Does a 2-piece knee keep both legs at least as spaced as the run (within 5%), without
    // pinching the bus narrower across the bend?
    let axis_first_keeps_gap = (0..m - 1).all(|pos| {
        let (ka, kb) = (idx[pos], idx[pos + 1]);
        let g_axis = (ns[kb] - ns[ka]) as f64;
        let g_diag = ((ne[kb] - me[kb]) - (ne[ka] - me[ka])) as f64 / sqrt2;
        g_axis >= 0.95 * d_run[pos] && g_diag >= 0.95 * d_run[pos]
    });
    let diag_first_keeps_gap = (0..m - 1).all(|pos| {
        let (ka, kb) = (idx[pos], idx[pos + 1]);
        let g_diag = ((ns[kb] - ms[kb]) - (ns[ka] - ms[ka])) as f64 / sqrt2;
        let g_axis = (ne[kb] - ne[ka]) as f64;
        g_diag >= 0.95 * d_run[pos] && g_axis >= 0.95 * d_run[pos]
    });

    // Prefer whichever 2-piece knee continues an adjacent track along any member's chain.
    let carries_count = |plans: &[MemberPlan]| -> usize {
        plans
            .iter()
            .filter(|p| {
                let ch = &all_chains[p.chain_idx];
                let (i, j) = p.span;
                (i > 0 && same_heading(ch.points[i - 1], p.path[0], p.path[0], p.path[1]))
                    || (j + 1 < ch.points.len()
                        && same_heading(p.path[p.path.len() - 2], p.path[p.path.len() - 1], p.path[p.path.len() - 1], ch.points[j + 1]))
            })
            .count()
    };

    let mut two_piece_good: Vec<Vec<MemberPlan>> = Vec::new();
    let mut two_piece_pinched: Vec<Vec<MemberPlan>> = Vec::new();
    for (pts, keeps_gap) in [(axis_first_pts, axis_first_keeps_gap), (diag_first_pts, diag_first_keeps_gap)] {
        if let Some(plans) = make_plans(pts) {
            if keeps_gap {
                two_piece_good.push(plans);
            } else {
                two_piece_pinched.push(plans);
            }
        }
    }
    two_piece_good.sort_by_key(|p| std::cmp::Reverse(carries_count(p)));
    two_piece_pinched.sort_by_key(|p| std::cmp::Reverse(carries_count(p)));

    // 3-piece staggered ways for a given target lane spacing `d_target`:
    //   Family A (axis -> 45-deg -> axis): 45-deg diagonals staggered by `sqrt(2) * d_target`.
    //   Family B (45-deg -> axis -> 45-deg): middle axis lanes staggered by `d_target`.
    let mut staggered: Vec<Vec<MemberPlan>> = Vec::new();
    let mut gap_targets: Vec<&[f64]> = vec![&d_run];
    if d_run.iter().zip(&d_min).any(|(&r, &mn)| r > mn + 5_000.0) {
        gap_targets.push(&d_min);
    }

    for target in gap_targets {
        // Family A: a_pos = a0 + (ns[k] - ns[k0]) - Phi[pos], where Phi[pos] = sum sqrt(2) * target.
        let k0 = idx[0];
        let mut offset_a = vec![0i64; m];
        let mut phi = 0.0;
        for pos in 1..m {
            phi += sqrt2 * target[pos - 1];
            let k = idx[pos];
            offset_a[pos] = (ns[k] - ns[k0]) - phi.round() as Nm;
        }
        // First try strictly interior Zs (every piece >= 2 * half_width), then boundary-anchored
        // staggered knees (f = 0.0 or 1.0, where the innermost track takes a 1-bend knee).
        let lo_inner: Nm = (0..m)
            .map(|pos| {
                let k = idx[pos];
                ms[k] + 2 * all_chains[members[k].chain_idx].half_width - offset_a[pos]
            })
            .max()
            .unwrap();
        let hi_inner: Nm = (0..m)
            .map(|pos| {
                let k = idx[pos];
                let dn = ne[k] - ns[k];
                (me[k] - dn) - 2 * all_chains[members[k].chain_idx].half_width - offset_a[pos]
            })
            .min()
            .unwrap();
        if lo_inner <= hi_inner {
            for f in [0.5, 0.25, 0.75] {
                let a0 = lo_inner + ((hi_inner - lo_inner) as f64 * f).round() as Nm;
                let pts: Vec<Vec<Point>> = (0..m)
                    .map(|pos| {
                        let k = idx[pos];
                        let a = a0 + offset_a[pos];
                        let dn = ne[k] - ns[k];
                        vec![from_mn(ms[k], ns[k]), from_mn(a, ns[k]), from_mn(a + dn, ne[k]), from_mn(me[k], ne[k])]
                    })
                    .collect();
                if let Some(plans) = make_plans(pts) {
                    staggered.push(plans);
                }
            }
        }
        let lo_full: Nm = (0..m).map(|pos| ms[idx[pos]] - offset_a[pos]).max().unwrap();
        let hi_full: Nm = (0..m)
            .map(|pos| {
                let k = idx[pos];
                (me[k] - (ne[k] - ns[k])) - offset_a[pos]
            })
            .min()
            .unwrap();
        if lo_full <= hi_full {
            for a0 in [lo_full, hi_full] {
                let pts: Vec<Vec<Point>> = (0..m)
                    .map(|pos| {
                        let k = idx[pos];
                        let a = a0 + offset_a[pos];
                        let dn = ne[k] - ns[k];
                        vec![from_mn(ms[k], ns[k]), from_mn(a, ns[k]), from_mn(a + dn, ne[k]), from_mn(me[k], ne[k])]
                    })
                    .collect();
                if let Some(plans) = make_plans(pts) {
                    staggered.push(plans);
                }
            }
        }

        // Family B: b_pos = b0 + B[pos], where B[pos] = sum target.
        let mut offset_b = vec![0i64; m];
        let mut acc = 0.0;
        for pos in 1..m {
            acc += target[pos - 1];
            offset_b[pos] = acc.round() as Nm;
        }
        let sqrt2_inv = std::f64::consts::FRAC_1_SQRT_2;
        let lo_b_in: Nm = (0..m)
            .map(|pos| {
                let k = idx[pos];
                let min_d = ((2 * all_chains[members[k].chain_idx].half_width) as f64 * sqrt2_inv).ceil() as Nm + 100;
                ns[k] + min_d - offset_b[pos]
            })
            .max()
            .unwrap();
        let hi_b_in: Nm = (0..m)
            .map(|pos| {
                let k = idx[pos];
                let min_d = ((2 * all_chains[members[k].chain_idx].half_width) as f64 * sqrt2_inv).ceil() as Nm + 100;
                ne[k] - min_d - offset_b[pos]
            })
            .min()
            .unwrap();
        if lo_b_in <= hi_b_in {
            for f in [0.5, 0.25, 0.75] {
                let b0 = lo_b_in + ((hi_b_in - lo_b_in) as f64 * f).round() as Nm;
                let pts: Vec<Vec<Point>> = (0..m)
                    .map(|pos| {
                        let k = idx[pos];
                        let b = b0 + offset_b[pos];
                        let (d1, d2) = (b - ns[k], ne[k] - b);
                        vec![from_mn(ms[k], ns[k]), from_mn(ms[k] + d1, b), from_mn(me[k] - d2, b), from_mn(me[k], ne[k])]
                    })
                    .collect();
                if let Some(plans) = make_plans(pts) {
                    staggered.push(plans);
                }
            }
        }
        let lo_b_full: Nm = (0..m).map(|pos| ns[idx[pos]] - offset_b[pos]).max().unwrap();
        let hi_b_full: Nm = (0..m).map(|pos| ne[idx[pos]] - offset_b[pos]).min().unwrap();
        if lo_b_full <= hi_b_full {
            for b0 in [lo_b_full, hi_b_full] {
                let pts: Vec<Vec<Point>> = (0..m)
                    .map(|pos| {
                        let k = idx[pos];
                        let b = b0 + offset_b[pos];
                        let (d1, d2) = (b - ns[k], ne[k] - b);
                        vec![from_mn(ms[k], ns[k]), from_mn(ms[k] + d1, b), from_mn(me[k] - d2, b), from_mn(me[k], ne[k])]
                    })
                    .collect();
                if let Some(plans) = make_plans(pts) {
                    staggered.push(plans);
                }
            }
        }
    }

    // Order: un-pinched 2-piece knees first, then exact-gap staggered ways, then pinched 2-piece
    // knees as a last fallback when clearance still permits them.
    out.extend(two_piece_good);
    out.extend(staggered);
    out.extend(two_piece_pinched);
}

/// Validates and optionally commits `plans` across all members simultaneously, all or none.
#[allow(clippy::type_complexity)]
fn try_bundle(
    board: &mut Board,
    all_chains: &[Chain],
    plans: &[MemberPlan],
    keep: bool,
) -> Option<Vec<(Vec<ItemId>, Vec<(ItemId, Item)>)>> {
    // Pure geometric pre-checks per member before mutating `board`.
    for p in plans {
        let ch = &all_chains[p.chain_idx];
        let (i, j) = p.span;
        // As in `try_way`: a Z's piece shorter than the track is wide grazes its neighbours.
        if p.path.len() > 3 && p.path.windows(2).any(|w| w[0].distance(w[1]) < (2 * ch.half_width) as f64) {
            return None;
        }
        // No piece may cross or touch the board's outline.
        let off_edge = p.path.windows(2).any(|w| {
            let seg = Shape::Capsule { a: w[0], b: w[1], r: 0 };
            let bbox = seg.bbox();
            board.outline_edges.iter().any(|(x, y, bb, _)| bb.overlaps(bbox) && seg.gap(&Shape::Capsule { a: *x, b: *y, r: 0 }) == 0.0)
        });
        if off_edge {
            return None;
        }
        // No member may gain doglegs, counting the adjacent tracks on either side of its span.
        let around = |pts: &[Point]| -> Vec<Point> {
            (i > 0)
                .then(|| ch.points[i - 1])
                .into_iter()
                .chain(pts.iter().copied())
                .chain((j + 1 < ch.points.len()).then(|| ch.points[j + 1]))
                .collect()
        };
        let was = doglegs(&around(&ch.points[i..=j]), ch.half_width);
        if doglegs(&around(&p.path), ch.half_width) > was {
            return None;
        }
    }

    // Record pre-removal same-net partners for every member.
    let mut partners_all: Vec<(Vec<ItemId>, Vec<ItemId>)> = Vec::with_capacity(plans.len());
    for p in plans {
        let ch = &all_chains[p.chain_idx];
        let layer = ch.layer.trailing_zeros() as usize;
        let (i, j) = p.span;
        let span = &ch.tracks[i..j];
        let mut partners: Vec<ItemId> = Vec::new();
        for &t in span {
            let track = board.item(t)?;
            partners.extend(
                board
                    .near(layer, track.shape.bbox())
                    .into_iter()
                    .filter(|&o| !span.contains(&o) && board.item(o).is_some_and(|it| it.net == ch.net) && joined(board, t, o)),
            );
        }
        partners.sort_unstable();
        partners.dedup();
        let sides: Vec<ItemId> = [
            i.checked_sub(1).map(|k| ch.tracks[k]),
            ch.tracks.get(j).copied(),
            p.pads[0],
            p.pads[1],
        ]
        .into_iter()
        .flatten()
        .collect();
        partners_all.push((partners, sides));
    }

    // Remove all members' old tracks together so neighbours do not block one another's knees.
    let mut removed_all: Vec<Vec<(ItemId, Item)>> = Vec::with_capacity(plans.len());
    let mut removed_ok = true;
    for p in plans {
        let ch = &all_chains[p.chain_idx];
        let span = &ch.tracks[p.span.0..p.span.1];
        let rem: Vec<(ItemId, Item)> = span.iter().filter_map(|&t| Some((t, board.remove(t)?))).collect();
        if rem.len() != span.len() {
            removed_ok = false;
        }
        removed_all.push(rem);
    }

    let mut pieces_all: Vec<Vec<ItemId>> = vec![Vec::new(); plans.len()];
    let mut ok = removed_ok;
    if ok {
        for (idx_p, p) in plans.iter().enumerate() {
            let ch = &all_chains[p.chain_idx];
            let r = if keep { ch.half_width } else { ch.half_width + TAUT_MARGIN };
            for w in p.path.windows(2) {
                let id = board.insert(Item {
                    kind: Kind::Track,
                    net: ch.net,
                    layers: ch.layer,
                    shape: Shape::Capsule { a: w[0], b: w[1], r },
                    hole: None,
                    locked: false,
                    span: None,
                });
                pieces_all[idx_p].push(id);
            }
        }
    }

    let mut why: Option<String> = None;
    if ok {
        for (idx_p, p) in plans.iter().enumerate() {
            let ch = &all_chains[p.chain_idx];
            let pieces = &pieces_all[idx_p];
            if let Some(v) = pieces.iter().find_map(|&id| check_item(board, id).into_iter().next()) {
                ok = false;
                why = Some(format!("rule net {} {v:?}", ch.net));
                break;
            }
            let (partners, sides) = &partners_all[idx_p];
            if !partners.iter().all(|&o| pieces.iter().chain(sides).any(|&s| s == o || joined(board, o, s))) {
                ok = false;
                why = Some(format!("join net {}", ch.net));
                break;
            }
            if !grazes(board, pieces).is_empty() {
                ok = false;
                why = Some(format!("graze net {}", ch.net));
                break;
            }
            if leaves_slot(board, ch, pieces, &[None, None]) {
                ok = false;
                why = Some(format!("slot net {}", ch.net));
                break;
            }
        }
    }

    if let Some(at) = square_trace() {
        let near = plans.iter().zip(&removed_all).any(|(p, rem)| {
            p.path.windows(2).any(|w| board::point_segment(at, w[0], w[1]) < 1e6)
                || rem.iter().any(|(_, it)| it.shape.bbox().grow(500_000).contains(at))
        });
        if near {
            let mm = |pt: &Point| format!("({:.3},{:.3})", pt.x as f64 / 1e6, pt.y as f64 / 1e6);
            let desc: Vec<String> = plans
                .iter()
                .map(|p| p.path.iter().map(mm).collect::<Vec<_>>().join("->"))
                .collect();
            let verdict = why.as_deref().unwrap_or("fits");
            eprintln!("squaring bundle [{}]{} -> {verdict}", desc.join(" | "), if keep { " (laid)" } else { "" });
        }
    }

    if ok && keep {
        return Some(pieces_all.into_iter().zip(removed_all).collect());
    }

    for pieces in pieces_all.into_iter().rev() {
        for id in pieces.into_iter().rev() {
            board.discard(id);
        }
    }
    for rem in removed_all.into_iter().rev() {
        for (id, item) in rem.into_iter().rev() {
            board.restore(id, item);
        }
    }
    ok.then(Vec::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cleanup::octilinear_watch;

    fn bus_board(nets: usize, clearance_mm: f64, track_mm: f64) -> Board {
        let mut sexp = String::from("(kicad_pcb (version 20240101) (general (thickness 1.6)) (layers (0 \"F.Cu\" signal)) (net 0 \"\")");
        for k in 1..=nets {
            sexp.push_str(&format!(" (net {k} \"D{}\")", k - 1));
        }
        sexp.push(')');
        let (pcb, copper) = kicad_pcb::parse_board(&sexp).unwrap();
        let pro = format!(
            r#"{{"net_settings":{{"classes":[{{"name":"Default","clearance":{clearance_mm},"track_width":{track_mm}}}]}}}}"#
        );
        let rules = kicad_pcb::Rules::parse(&pro).unwrap();
        Board::new(&pcb, &copper, rules)
    }

    fn rect_pad(net: u32, cx: Nm, cy: Nm, hx: Nm, hy: Nm) -> Item {
        Item {
            kind: Kind::Pad { footprint: 0, number: "1".into(), plated: false },
            net,
            layers: 1,
            shape: Shape::Polygon(vec![
                Point::new(cx - hx, cy - hy),
                Point::new(cx + hx, cy - hy),
                Point::new(cx + hx, cy + hy),
                Point::new(cx - hx, cy + hy),
            ]),
            hole: None,
            locked: true,
            span: None,
        }
    }

    fn track(net: u32, a: Point, b: Point, r: Nm) -> Item {
        Item { kind: Kind::Track, net, layers: 1, shape: Shape::Capsule { a, b, r }, hole: None, locked: false, span: None }
    }

    #[test]
    fn eight_bit_qfp_bus_squares_together_with_equal_bend_gap_where_single_chain_cannot() {
        // Two 0.5 mm pitch QFP columns 16 mm apart horizontally and shifted 6 mm vertically
        // (20.56 degrees). Rectangular SMD pads forbid 45-degree entry (`slants_into`), and every
        // single-track Z (`f = 0.5, 0.25, 0.75`) pokes its knee across the neighbour's slanted
        // line, so single-chain squaring alone bends 0 of the 8 tracks.
        let mut bundle_board = bus_board(8, 0.15, 0.20);
        let mut bundle_soft = Vec::new();

        for k in 0..8usize {
            let net = (k + 1) as u32;
            let y0 = (k as Nm) * 500_000;
            let (s, e) = (Point::new(0, y0), Point::new(16_000_000, y0 + 6_000_000));
            bundle_board.insert(rect_pad(net, s.x - 300_000, s.y, 450_000, 120_000));
            bundle_board.insert(rect_pad(net, e.x + 300_000, e.y, 450_000, 120_000));
            bundle_soft.push(bundle_board.insert(track(net, s, e, 100_000)));
        }
        assert!(board::check(&bundle_board).is_empty(), "{:?}", board::check(&bundle_board));

        let bent = square_bundles(&mut bundle_board, &mut bundle_soft, &Default::default(), &mut |_, _, _, _| {});
        assert_eq!(bent, 8, "all 8 bus tracks must bend together");
        assert!(board::check(&bundle_board).is_empty(), "{:?}", board::check(&bundle_board));
        assert!(board::connectivity(&bundle_board).airlines.is_empty());
        assert!(grazes(&bundle_board, &bundle_soft).is_empty());

        // Every remaining track is strictly octilinear (0 off-45 segments), and the 8 diagonal
        // 45-degree pieces have equal perpendicular spacing matching the slanted run's gap.
        let mut diag_intercepts: Vec<f64> = Vec::new();
        for &id in &bundle_soft {
            let Shape::Capsule { a, b, .. } = bundle_board.item(id).expect("laid").shape else { panic!("track") };
            assert!(!is_off45(a, b), "left off-45 piece {a:?} -> {b:?}");
            let (dx, dy) = ((b.x - a.x).abs(), (b.y - a.y).abs());
            if dx > 0 && dy > 0 {
                assert_eq!(dx, dy, "diagonal piece must be exact 45 degrees");
                diag_intercepts.push((a.y - a.x) as f64 / std::f64::consts::SQRT_2);
            }
        }
        assert_eq!(diag_intercepts.len(), 8, "one 45-degree middle piece per net");
        diag_intercepts.sort_by(|a, b| a.total_cmp(b));

        // Slanted run pitch was 0.5 mm * cos(atan2(6, 16)) = 468.165 um.
        let expected_cc = 500_000.0 * (16.0_f64 / 16.0_f64.hypot(6.0));
        for w in diag_intercepts.windows(2) {
            let cc = w[1] - w[0];
            assert!(
                (cc - expected_cc).abs() <= 2_000.0,
                "diagonal centre-to-centre gap {cc:.1} nm should match run gap {expected_cc:.1} nm"
            );
        }
    }

    #[test]
    fn bundle_move_is_all_or_none_when_an_obstacle_blocks_every_group_way() {
        // Two tightly spaced slanted tracks between rectangular pads, with locked vias blocking
        // every staggered Z along the corridor: neither track may move alone, and the group move
        // must roll back cleanly leaving the original track ids intact.
        let mut board = bus_board(3, 0.15, 0.20);
        let mut soft = Vec::new();
        for k in 0..2usize {
            let net = (k + 1) as u32;
            let y0 = (k as Nm) * 420_000;
            let (s, e) = (Point::new(0, y0), Point::new(10_000_000, y0 + 4_000_000));
            board.insert(rect_pad(net, s.x - 250_000, s.y, 350_000, 110_000));
            board.insert(rect_pad(net, e.x + 250_000, e.y, 350_000, 110_000));
            soft.push(board.insert(track(net, s, e, 100_000)));
        }
        // Place locked vias of net 3 close below net 1 (`y = 0.4 * x`) and above net 2
        // (`y = 0.42 + 0.4 * x`) so the slanted pair fits with ~0.18 mm clearance, while any
        // axis-diagonal-axis or diagonal-axis-diagonal deflection hits a via.
        for (vx, vy) in [
            (2_000_000, 300_000),
            (4_000_000, 1_100_000),
            (6_000_000, 1_900_000),
            (8_000_000, 2_700_000),
            (2_000_000, 1_720_000),
            (4_000_000, 2_520_000),
            (6_000_000, 3_320_000),
            (8_000_000, 4_120_000),
        ] {
            let at = Point::new(vx, vy);
            board.insert(Item {
                kind: Kind::Via,
                net: 0,
                layers: 1,
                shape: Shape::Capsule { a: at, b: at, r: 180_000 },
                hole: Some((at, 90_000)),
                locked: true,
                span: None,
            });
        }
        assert!(board::check(&board).is_empty(), "{:?}", board::check(&board));
        let original = soft.clone();
        let bent = octilinear_watch(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {});
        assert_eq!(bent, 0, "blocked bundle must roll back all or none");
        assert_eq!(soft, original, "original track ids must remain untouched");
        assert!(board::check(&board).is_empty());
        assert!(board::connectivity(&board).airlines.is_empty());
    }

    #[test]
    fn multi_segment_slanted_fanout_curves_expand_and_square_into_aligned_knees() {
        // Modelled on APM-RPi-Shield's IC2-Pad18..21: four 0.4 mm tracks leaving vertical pads at
        // 0.65 mm pitch through two off-45 taut segments into parallel 135-degree runs. Individual
        // knees collide into the neighbour's slanted bend; expanding the contiguous off-45 spans
        // squares all four together into aligned vertical + 135-degree knees.
        let mut board = bus_board(4, 0.10, 0.40);
        let mut soft = Vec::new();
        let r = 200_000;
        let specs = [
            (72_625_000, 96_750_000, 72_543_000, 97_693_000, 72_443_000, 97_893_000, 71_943_000, 98_393_000),
            (73_275_000, 96_750_000, 73_193_000, 97_693_000, 72_993_000, 98_193_000, 72_143_000, 99_043_000),
            (73_925_000, 96_750_000, 73_843_000, 97_693_000, 73_543_000, 98_493_000, 71_943_000, 100_093_000),
            (74_575_000, 96_750_000, 74_493_000, 97_693_000, 74_093_000, 98_793_000, 70_793_000, 102_093_000),
        ];
        for (idx, (x0, y0, x1, y1, x2, y2, x3, y3)) in specs.into_iter().enumerate() {
            let net = (idx + 1) as u32;
            let (p0, p1, p2, p3) = (Point::new(x0, y0), Point::new(x1, y1), Point::new(x2, y2), Point::new(x3, y3));
            let p4 = Point::new(68_000_000, y3);
            board.insert(rect_pad(net, p0.x, p0.y - 300_000, 180_000, 450_000));
            board.insert(rect_pad(net, p4.x - 200_000, p4.y, 300_000, 180_000));
            soft.push(board.insert(track(net, p0, p1, r)));
            soft.push(board.insert(track(net, p1, p2, r)));
            soft.push(board.insert(track(net, p2, p3, r)));
            soft.push(board.insert(track(net, p3, p4, r)));
        }
        assert!(board::check(&board).is_empty(), "{:?}", board::check(&board));
        let bent = square_bundles(&mut board, &mut soft, &Default::default(), &mut |_, _, _, _| {});
        assert_eq!(bent, 4, "all 4 fanout chains must square together");
        assert!(board::check(&board).is_empty(), "{:?}", board::check(&board));
        assert!(board::connectivity(&board).airlines.is_empty());
        for &id in &soft {
            let Shape::Capsule { a, b, .. } = board.item(id).expect("laid").shape else { panic!("track") };
            assert!(!is_off45(a, b), "left off-45 piece {a:?} -> {b:?}");
        }
    }
}
