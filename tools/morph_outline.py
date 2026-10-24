#!/usr/bin/env python3
"""Plan a continuous board-shape morph in outline mode: outline, parts and a copper preview, no DRC.

The shape path is one continuous family. A superellipse |x/a|^n + |y/b|^n = 1 sweeps
rectangle (large n) -> circle (n = 2) -> rounded diamond (n ~ 1.3) -> stretched rounded rectangle,
then that band is bent along its length into a rainbow arch. Every outline is sampled with the same
number of points, starting at the bottom anchor (USB) with the top anchor (radio) half way round,
so point i of one outline corresponds to point i of every other.

Interior points (part centres, copper corners) follow mean-value coordinates from the starting
rectangle to each superellipse, then the exact bend map. Parts translate rigidly with their centre;
the anchors are parts like any other, and ride the ends because the ends are where they sit.

Each anchor carries its fan-in rigidly: the parts within --fan-in-mm that share a net other than
ground with it, and the parts among those, keep their place and turn relative to the anchor, so the copper between them (USB's
pair into its ESD part, say) is never squeezed.

Parts turn with the map's local rotation. Writes frames for the morph-frames viewer and plan.json
(per step: outline, and per part [dx, dy, turn in degrees] from where it starts).

    morph_outline.py JOB_DIR OUT_DIR [--anchors J1,U7] [--fan-in-mm 9.2] [--steps 30]
"""
import argparse
import json
import math
import re
from pathlib import Path

import numpy as np

N = 256
LAYER_COLOURS = {"F.Cu": "#f26d78", "B.Cu": "#68b8ff", "In2.Cu": "#ce9fff"}


# ----------------------------------------------------------------------------- reading the job

def sexp_blocks(text, head):
    """Top-level-ish `(head ...)` blocks, with their balanced text."""
    out, i = [], 0
    while True:
        i = text.find("(" + head, i)
        if i < 0:
            return out
        depth, j = 0, i
        while True:
            c = text[j]
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        out.append(text[i:j + 1])
        i = j + 1


def pad_radii(dsn_text):
    """Each padstack's largest extent from its centre, in DSN units."""
    radii = {}
    for block in sexp_blocks(dsn_text, "padstack "):
        name = re.match(r'\(padstack\s+("[^"]*"|\S+)', block).group(1).strip('"')
        r = 0.0
        for m in re.finditer(r"\(circle\s+\S+\s+([-\d.]+)", block):
            r = max(r, float(m.group(1)) / 2)
        for m in re.finditer(r"\((?:rect|polygon|path)\s+\S+\s+([-\d.\s]+)\)", block):
            v = list(map(float, m.group(1).split()))
            v = v[1:] if "(polygon" in m.group(0) or "(path" in m.group(0) else v
            r = max([r] + [math.hypot(x, y) for x, y in zip(v[::2], v[1::2])])
        radii[name] = r
    return radii


def read_parts(dsn_text, unit_mm, margin_mm=0.15):
    """Each placed part as its centre and the box around its pads (pad extent plus `margin_mm`):
    footprint outlines can't be told apart from keepouts in a DSN, so they don't count."""
    radii = pad_radii(dsn_text)
    images = {}
    for block in sexp_blocks(dsn_text, "image "):
        name = re.match(r'\(image\s+("[^"]*"|\S+)', block).group(1).strip('"')
        pts = []
        for m in re.finditer(r'\(pin\s+("[^"]*"|\S+)\s+(?:\(rotate\s+[-\d.]+\)\s+)?("[^"]*"|\S+)\s+([-\d.]+)\s+([-\d.]+)', block):
            r = radii.get(m.group(1).strip('"'), 0.0) + margin_mm / unit_mm
            x, y = float(m.group(3)), float(m.group(4))
            pts += [(x - r, y - r), (x + r, y + r)]
        if pts:
            images[name] = np.array(pts) * unit_mm
    parts = {}
    for block in sexp_blocks(dsn_text, "component "):
        image = re.match(r'\(component\s+("[^"]*"|\S+)', block).group(1).strip('"')
        for m in re.finditer(r'\(place\s+("[^"]*"|\S+)\s+([-\d.]+)\s+([-\d.]+)\s+(front|back)\s+([-\d.]+)', block):
            ref = m.group(1).strip('"')
            x, y = float(m.group(2)) * unit_mm, float(m.group(3)) * unit_mm
            side, rot = m.group(4), math.radians(float(m.group(5)))
            local = images.get(image, np.zeros((1, 2))).copy()
            if side == "back":
                local[:, 0] = -local[:, 0]
            c, s = math.cos(rot), math.sin(rot)
            world = np.column_stack([local[:, 0] * c - local[:, 1] * s + x, local[:, 0] * s + local[:, 1] * c + y])
            lo, hi = world.min(axis=0), world.max(axis=0)
            parts[ref] = {"centre": np.array([x, y]), "box": (lo - [x, y], hi - [x, y]), "side": side}
    return parts


def read_outline(dsn_text, unit_mm):
    boundary = sexp_blocks(dsn_text, "boundary")[0]
    v = list(map(float, re.search(r"\(path\s+\S+\s+[-\d.]+\s+([-\d.\s]+)\)", boundary).group(1).split()))
    return np.array(list(zip(v[::2], v[1::2]))) * unit_mm


def read_wires(text, unit_mm, section):
    start = text.find("(" + section)
    if start < 0:
        return []
    wires = []
    for m in re.finditer(r"\(path\s+(\S+)\s+([-\d.]+)\s+([-\d.\s]+)\)", text[start:]):
        v = list(map(float, m.group(3).split()))
        wires.append((m.group(1).strip('"'), float(m.group(2)) * unit_mm, np.array(list(zip(v[::2], v[1::2]))) * unit_mm))
    return wires


def unit_of(text):
    m = re.search(r"\(resolution\s+(\w+)\s+([\d.]+)\)", text)
    per = {"um": 0.001, "mm": 1.0, "mil": 0.0254, "inch": 25.4}[m.group(1)]
    return per / float(m.group(2)) if "(session" in text[:200] else per


# ----------------------------------------------------------------------------- shapes

def superellipse(a, b, n, centre):
    """N points, arc-length uniform on each half, starting at the bottom, counter-clockwise."""
    t = np.linspace(0, 2 * math.pi, 4096, endpoint=False) - math.pi / 2
    c, s = np.cos(t), np.sin(t)
    # cos(±90°) is 6e-17, not 0, and a large n would lift it to millimetres.
    c[np.abs(c) < 1e-9] = 0.0
    s[np.abs(s) < 1e-9] = 0.0
    pts = np.column_stack([a * np.sign(c) * np.abs(c) ** (2 / n), b * np.sign(s) * np.abs(s) ** (2 / n)])
    return resample(pts) + centre


def resample(pts):
    """Arc-length uniform within each half (bottom -> top on the right, top -> bottom on the left),
    so the bottom and top points stay at indices 0 and N/2."""
    top = int(np.argmax(pts[:, 1]))
    halves = [pts[: top + 1], np.vstack([pts[top:], pts[:1]])]
    out = []
    for half in halves:
        d = np.r_[0, np.cumsum(np.linalg.norm(np.diff(half, axis=0), axis=1))]
        target = np.linspace(0, d[-1], N // 2 + 1)[:-1]
        out.append(np.column_stack([np.interp(target, d, half[:, 0]), np.interp(target, d, half[:, 1])]))
    return np.vstack(out)


def from_bottom(poly):
    """The polygon counter-clockwise, starting at its lowest point."""
    x, y = poly[:, 0], poly[:, 1]
    if np.dot(x, np.roll(y, -1)) - np.dot(y, np.roll(x, -1)) < 0:
        poly = poly[::-1]
    return np.roll(poly, -int(np.argmin(poly[:, 1])), axis=0)


def area(poly):
    x, y = poly[:, 0], poly[:, 1]
    return 0.5 * abs(np.dot(x, np.roll(y, -1)) - np.dot(y, np.roll(x, -1)))


def bend(points, beta, bottom, length, centre_x):
    """Bends the vertical band [bottom, bottom + length] into an arc of angle beta, curling to +x."""
    if beta < 1e-6:
        return points.copy()
    r = length / beta
    u = points[:, 1] - bottom
    v = points[:, 0] - centre_x
    theta = u / r
    out = np.column_stack([centre_x + r - (r - v) * np.cos(theta), bottom + (r - v) * np.sin(theta)])
    return out


# ----------------------------------------------------------------------------- mapping

def mean_value_map(points, source, target):
    """Mean-value coordinates of `points` in polygon `source`, applied to polygon `target`."""
    d = source[None, :, :] - points[:, None, :]
    r = np.linalg.norm(d, axis=2) + 1e-12
    dn = np.roll(d, -1, axis=1)
    rn = np.roll(r, -1, axis=1)
    cross = d[:, :, 0] * dn[:, :, 1] - d[:, :, 1] * dn[:, :, 0]
    dot = (d * dn).sum(axis=2)
    tan_half = np.abs(cross) / (r * rn + dot + 1e-12)
    tan_half = np.where(cross >= 0, tan_half, -tan_half)
    w = (np.roll(tan_half, 1, axis=1) + tan_half) / r
    w /= w.sum(axis=1, keepdims=True)
    return w @ target


def inside(points, poly):
    x, y = points[:, 0][:, None], points[:, 1][:, None]
    x0, y0 = poly[:, 0][None, :], poly[:, 1][None, :]
    x1, y1 = np.roll(poly[:, 0], -1)[None, :], np.roll(poly[:, 1], -1)[None, :]
    crosses = ((y0 > y) != (y1 > y)) & (x < (x1 - x0) * (y - y0) / (y1 - y0 + 1e-12) + x0)
    return crosses.sum(axis=1) % 2 == 1


# ----------------------------------------------------------------------------- plan

def smooth(t):
    return t * t * (3 - 2 * t)


def keyframes(outline, target_area):
    lo, hi = outline.min(axis=0), outline.max(axis=0)
    centre = (lo + hi) / 2
    a0, b0 = (hi - lo) / 2

    def fit(n, aspect):
        """Semi-axes of a superellipse of exponent n and b/a = aspect with the target area."""
        unit = area(superellipse(1.0, aspect, n, np.zeros(2)))
        a = math.sqrt(target_area / unit)
        return a, a * aspect

    circle = fit(2.0, 1.0)
    diamond = fit(1.3, 1.35)
    stretched = fit(5.0, 4.6)
    return centre, [
        ("rectangle", a0, b0, 40.0),
        ("circle", *circle, 2.0),
        ("rounded diamond", *diamond, 1.3),
        ("stretched rounded rectangle", *stretched, 5.0),
    ]


def plan(outline, steps, bend_steps):
    lo, hi = outline.min(axis=0), outline.max(axis=0)
    centre, keys = keyframes(outline, (hi - lo).prod())
    out = []
    for k in range(len(keys) - 1):
        (name0, a0, b0, n0), (name1, a1, b1, n1) = keys[k], keys[k + 1]
        for i in range(steps + (1 if k == 0 else 0)):
            t = smooth((i + (0 if k == 0 else 1)) / steps)
            n = math.exp((1 - t) * math.log(n0) + t * math.log(n1))
            a, b = (1 - t) * a0 + t * a1, (1 - t) * b0 + t * b1
            out.append({"label": f"{name0} → {name1}" if 0 < t < 1 else (name0 if t == 0 else name1),
                        "a": a, "b": b, "n": n, "beta": 0.0})
    last = out[-1]
    for i in range(1, bend_steps + 1):
        out.append({**last, "beta": math.pi * smooth(i / bend_steps),
                    "label": "rainbow" if i == bend_steps else "bending → rainbow"})
    return centre, keys[0], out


def realise(step, centre, start_outline, points):
    """The step's outline, `points` carried onto it, and how far the bend turns each point
    (radians; parts turn with the arch only)."""
    poly = superellipse(step["a"], step["b"], step["n"], centre)
    mapped = mean_value_map(points, start_outline, poly)
    turn = np.zeros(len(points))
    if step["beta"] > 0:
        bottom, length = centre[1] - step["b"], 2 * step["b"]
        turn = -(mapped[:, 1] - bottom) * step["beta"] / length
        poly = bend(poly, step["beta"], bottom, length, centre[0])
        mapped = bend(mapped, step["beta"], bottom, length, centre[0])
        shift = centre[0] - (poly[:, 0].min() + poly[:, 0].max()) / 2
        poly, mapped = poly + [shift, 0], mapped + [shift, 0]
    return poly, mapped, turn


# ----------------------------------------------------------------------------- frames

def svg_frame(view, poly, parts, positions, angles, wires_mapped, overlaps, outside, anchors, start_poly):
    x0, y0, w, h = view
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x0:.3f} {y0:.3f} {w:.3f} {h:.3f}">',
           f'<rect x="{x0:.3f}" y="{y0:.3f}" width="{w:.3f}" height="{h:.3f}" fill="#101c25"/>']
    pts = lambda p: " ".join(f"{x:.3f},{-y:.3f}" for x, y in p)
    out.append(f'<polygon points="{pts(start_poly)}" fill="none" stroke="#3a4a55" stroke-width="0.15" stroke-dasharray="0.6 0.6"/>')
    out.append(f'<polygon points="{pts(poly)}" fill="#13242f" stroke="#83c5af" stroke-width="0.3"/>')
    for layer, width, line in wires_mapped:
        out.append(f'<polyline points="{pts(line)}" fill="none" stroke="{LAYER_COLOURS.get(layer, "#888")}" '
                   f'stroke-width="{max(width, 0.1):.3f}" stroke-opacity="0.55" stroke-linecap="round" stroke-linejoin="round"/>')
    for ref, part in parts.items():
        fill = "#edcf86" if part["side"] == "front" else "#9fb7c8"
        stroke = "#ffd84d" if ref in anchors else ("#ff6b60" if ref in overlaps or ref in outside else "none")
        opacity = 0.45 if part["side"] == "front" else 0.3
        box = corners(parts, ref, positions[ref], angles.get(ref, 0.0))
        out.append(f'<polygon points="{pts(box)}" '
                   f'fill="{fill}" fill-opacity="{opacity}" stroke="{stroke}" stroke-width="0.25"/>')
    out.append("</svg>")
    return "".join(out)


def corners(parts, ref, position, angle):
    """The part's pad box, turned by `angle` (radians) about its centre, in board coordinates."""
    lo, hi = parts[ref]["box"]
    local = np.array([lo, [hi[0], lo[1]], hi, [lo[0], hi[1]]])
    c, s = math.cos(angle), math.sin(angle)
    return position + local @ np.array([[c, s], [-s, c]])


def overlap_pairs(parts, positions, angles=None, margin=0.05):
    """Same-side part pairs whose turned boxes overlap by more than `margin` (separating axes),
    each with its least-overlap axis (unit, pointing from the first part to the second) and depth."""
    refs = list(parts)
    angles = angles or {}
    world = np.array([corners(parts, r, positions[r], angles.get(r, 0.0)) for r in refs])
    lo, hi = world.min(axis=1), world.max(axis=1)
    side = np.array([parts[r]["side"] == "front" for r in refs])
    rough = (np.minimum(hi[:, None, :], hi[None, :, :]) - np.maximum(lo[:, None, :], lo[None, :, :]) > margin).all(axis=2)
    rough &= (side[:, None] == side[None, :]) & np.triu(np.ones_like(rough, dtype=bool), 1)
    out = {}
    for i, j in zip(*np.nonzero(rough)):
        a, b = world[i], world[j]
        best, axis_best = None, None
        for poly in (a, b):
            for k in range(2):
                edge = poly[k + 1] - poly[k]
                axis = np.array([-edge[1], edge[0]]) / (np.linalg.norm(edge) + 1e-12)
                pa, pb = a @ axis, b @ axis
                depth = min(pa.max(), pb.max()) - max(pa.min(), pb.min())
                if best is None or depth < best:
                    best = depth
                    axis_best = axis if pb.mean() >= pa.mean() else -axis
        if best > margin:
            out[(refs[i], refs[j])] = (axis_best, best)
    return out


def nearest_inside(point, poly):
    """The closest point of the outline to `point`, and the inward normal there (counter-clockwise
    outline)."""
    a, b = poly, np.roll(poly, -1, axis=0)
    e = b - a
    t = np.clip(((point - a) * e).sum(axis=1) / ((e * e).sum(axis=1) + 1e-12), 0, 1)
    q = a + e * t[:, None]
    k = int(np.argmin(np.linalg.norm(q - point, axis=1)))
    n = np.array([-e[k, 1], e[k, 0]]) / (np.linalg.norm(e[k]) + 1e-12)
    return q[k], n


def outside_parts(parts, positions, poly, angles=None):
    refs = list(parts)
    angles = angles or {}
    world = np.vstack([corners(parts, r, positions[r], angles.get(r, 0.0)) for r in refs])
    ins = inside(world, poly).reshape(len(refs), 4)
    return {r for k, r in enumerate(refs) if not ins[k].all()}


def fan_in(dsn_text, parts, anchors, radius):
    """Each anchor's fan-in: the parts within `radius` of it sharing a net other than ground, and
    every part sitting among them (centre inside their bounding box, 1 mm grown)."""
    nets = [{p.rsplit("-", 1)[0] for p in pins.split()}
            for name, pins in re.findall(r'\(net\s+("[^"]*"|\S+)\s*\(pins\s+([^)]*)\)', dsn_text)
            if name.strip('"').upper() not in ("GND", "AGND", "DGND")]
    groups = {}
    for a in anchors:
        centre = np.asarray(parts[a]["centre"])
        groups[a] = sorted(r for r in parts if r not in anchors
                           and np.linalg.norm(np.asarray(parts[r]["centre"]) - centre) <= radius
                           and any(a in ps and r in ps for ps in nets))
        box = np.array([parts[r]["centre"] for r in [a] + groups[a]])
        lo, hi = box.min(axis=0) - 1.0, box.max(axis=0) + 1.0
        groups[a] += sorted(r for r in parts if r not in anchors and r not in groups[a]
                            and (lo <= parts[r]["centre"]).all() and (parts[r]["centre"] <= hi).all())
    return groups


def relax(parts, positions, poly, fixed, allowed_pairs, allowed_out, angles=None, rounds=800):
    """Pushes overlapping parts apart and stray parts back inside the outline, moving nothing in
    `fixed`; pairs and parts already in conflict on the original board are left as they are."""
    positions = {r: p.copy() for r, p in positions.items()}
    for _ in range(rounds):
        moved = False
        # All pushes at once and damped: one pair at a time oscillates in a jam.
        shift = {r: np.zeros(2) for r in positions}
        for (a, b), (along, depth) in overlap_pairs(parts, positions, angles).items():
            if (a, b) in allowed_pairs:
                continue
            wa, wb = (0.0 if a in fixed else 1.0), (0.0 if b in fixed else 1.0)
            if wa + wb == 0:
                continue
            apart = positions[b] - positions[a]
            apart = apart / (np.linalg.norm(apart) + 1e-9)
            direction = along + 0.5 * apart
            direction /= np.linalg.norm(direction) + 1e-9
            push = direction * (depth + 0.08) / max(float(direction @ along), 0.3)
            shift[a] -= push * wa / (wa + wb)
            shift[b] += push * wb / (wa + wb)
            moved = True
        for r, v in shift.items():
            positions[r] = positions[r] + 0.6 * v
        for r in outside_parts(parts, positions, poly, angles) - allowed_out - set(fixed):
            box = corners(parts, r, positions[r], (angles or {}).get(r, 0.0))
            out = box[~inside(box, poly)]
            worst, best = None, 0.0
            for c in out:
                q, n = nearest_inside(c, poly)
                if np.linalg.norm(q - c) >= best:
                    worst, best = (q - c) + n * 0.05, np.linalg.norm(q - c)
            positions[r] = positions[r] + worst
            moved = True
        if not moved:
            break
    return positions


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("job")
    ap.add_argument("out")
    ap.add_argument("--anchors", default="J1,U7")
    ap.add_argument("--fan-in-mm", type=float, default=9.2)
    ap.add_argument("--steps", type=int, default=30)
    args = ap.parse_args()

    job, out = Path(args.job), Path(args.out)
    dsn_path = next((job / "input").glob("*.dsn"))
    dsn = dsn_path.read_text(errors="replace")
    unit = unit_of(dsn)
    parts = read_parts(dsn, unit)
    outline = read_outline(dsn, unit)
    wires = read_wires(dsn, unit, "wiring")
    ses = job / "best" / "best.ses"
    if ses.exists():
        text = ses.read_text(errors="replace")
        wires += read_wires(text, unit_of(text), "routes")
    anchors = [a for a in args.anchors.split(",") if a in parts]
    groups = fan_in(dsn, parts, anchors, args.fan_in_mm)
    rigid = list(anchors) + [r for members in groups.values() for r in members]
    print("fan-in carried with the anchors:", groups)

    centre, (_, a0, b0, n0), steps = plan(outline, args.steps, args.steps + 10)
    start = superellipse(a0, b0, n0, centre)

    def subdivide(line, step=0.5):
        pieces = [line[:1]]
        for p, q in zip(line, line[1:]):
            k = max(1, int(math.ceil(np.linalg.norm(q - p) / step)))
            pieces.append(p + (q - p) * (np.arange(1, k + 1)[:, None] / k))
        return np.vstack(pieces)

    fine = [(layer, w, subdivide(line)) for layer, w, line in wires]
    refs = list(parts)
    centres = np.array([parts[r]["centre"] for r in refs])
    copper_pts = np.vstack([line for _, _, line in fine]) if fine else np.zeros((0, 2))
    splits = np.cumsum([len(line) for _, _, line in fine])[:-1]
    n = len(refs)

    polys = []
    for step in steps:
        poly, mapped, turn = realise(step, centre, start, np.vstack([centres, copper_pts]))
        polys.append((poly, mapped, turn[:n]))
    every = np.vstack([p for p, _, _ in polys] + [outline])
    lo, hi = every.min(axis=0) - 3, every.max(axis=0) + 3
    view = (lo[0], -hi[1], hi[0] - lo[0], hi[1] - lo[1])

    out.mkdir(parents=True, exist_ok=True)
    for old in out.glob("*.svg"):
        old.unlink()
    lines, plan_steps = [], []
    original = {r: parts[r]["centre"] for r in refs}
    allowed_pairs = set(overlap_pairs(parts, original))
    allowed_out = outside_parts(parts, original, outline)
    carried = None
    for i, (step, (poly, mapped, turn)) in enumerate(zip(steps, polys)):
        positions = {r: mapped[k] for k, r in enumerate(refs)}
        angles = {r: float(turn[k]) for k, r in enumerate(refs)}
        copper = np.split(mapped[len(refs):], splits) if fine else []
        wires_mapped = [(layer, w, c) for (layer, w, _), c in zip(fine, copper)]
        if carried is not None:
            positions = {r: positions[r] + carried[r] for r in refs}
        for a, members in groups.items():
            c, s_ = math.cos(angles[a]), math.sin(angles[a])
            for r in members:
                offset = np.asarray(parts[r]["centre"]) - np.asarray(parts[a]["centre"])
                positions[r] = positions[a] + np.array([c * offset[0] - s_ * offset[1], s_ * offset[0] + c * offset[1]])
                angles[r] = angles[a]
        relaxed = relax(parts, positions, poly, rigid, allowed_pairs, allowed_out, angles)
        carried = {r: relaxed[r] - positions[r] + (carried[r] if carried is not None else 0) for r in refs}
        positions = relaxed
        pairs = {p for p in overlap_pairs(parts, positions, angles) if p not in allowed_pairs}
        overlaps = {r for p in pairs for r in p}
        outside = outside_parts(parts, positions, poly, angles) - allowed_out
        file = f"{i:05d}.svg"
        (out / file).write_text(svg_frame(view, poly, parts, positions, angles, wires_mapped, overlaps, outside, anchors, start))
        moved = {r: (positions[r] - parts[r]["centre"]).round(4).tolist() + [round(math.degrees(angles[r]), 3)]
                 for r in refs}
        anchor_gap = float(np.linalg.norm(positions[anchors[0]] - positions[anchors[1]])) if len(anchors) == 2 else None
        lines.append({
            "frame": i, "file": file, "operators": ["outline", step["label"]], "parts": sorted(overlaps | outside),
            "by_mm": [0.0, 0.0], "unconnected": len(overlaps), "open": [],
            "stats": {"shape": step["label"], "relaxed mm (max)": round(max(np.linalg.norm(v) for v in carried.values()), 2), "area mm²": round(area(poly), 1), "n": round(step["n"], 2),
                      "bend °": round(math.degrees(step["beta"]), 1),
                      "largest part turn °": round(max(abs(math.degrees(a)) for a in angles.values()), 1),
                      "overlapping parts": len(overlaps), "parts past the edge": len(outside),
                      "anchor distance mm": round(anchor_gap, 2) if anchor_gap else None},
        })
        plan_steps.append({"label": step["label"], "outline": poly.round(4).tolist(), "parts": moved})
    (out / "frames.jsonl").write_text("".join(json.dumps(l) + "\n" for l in lines))
    (out / "plan.json").write_text(json.dumps({
        "anchors": anchors, "centre": centre.tolist(),
        "board_outline": resample(from_bottom(outline)).round(4).tolist(),
        "view": [lo[0], lo[1], hi[0], hi[1]],
        "steps": plan_steps}))
    worst = max(lines, key=lambda l: l["stats"]["overlapping parts"])
    print(f"{len(lines)} steps; worst overlap {worst['stats']['overlapping parts']} parts at step {worst['frame']} "
          f"({worst['stats']['shape']}); parts past the edge peak {max(l['stats']['parts past the edge'] for l in lines)}")


if __name__ == "__main__":
    main()
