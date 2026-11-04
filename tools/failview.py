#!/usr/bin/env python3
"""Render visual failure crops and whole-board overviews for open connections in a routed run.

Single board:
    python3 tools/failview.py RUN_DIR/ID [--board BOARD_DIR] [--report REPORT.json] -o OUT_DIR

Whole run (all failed boards in RUN_DIR, writing OUT_DIR/<ID>/ and OUT_DIR/index.json):
    python3 tools/failview.py RUN_DIR [--boards-dir DIR] [--reports-dir DIR] [--causes FILE] [-o OUT_DIR]

For each unconnected item in out.drc.json, failview writes an SVG and PNG crop centred on the open
connection (plus zoom insets around both ends when the connection spans far across the board), a
per-layer split view so overlapping copper never hides a block, and one whole-board overview with
every open airwire marked. Geometry is read through KiCad's pcbnew API so rotated footprints,
custom pad shapes, offset drills, arcs, zones and keepouts match KiCad's own DRC.
"""

import argparse
import concurrent.futures
import fnmatch
import json
import math
import os
import re
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path
from xml.sax.saxutils import escape as xml_esc

try:
    import wx
    wx.Log.EnableLogging(False)
except Exception:
    pass

import cairosvg
import pcbnew

ROOT = Path(__file__).resolve().parent.parent
LAB = Path(os.environ.get("ULTRAROUTE_LAB", Path.home() / "Software" / "ultraroute-lab"))
DEFAULT_BOARDS = LAB / "pcbench" / "boards"

MM = 1e6
POLY_ERR = 5000  # 5 um chord tolerance when polygonising curved copper or pads

# Consistent copper palette: red front, blue back, distinct hues on inner layers.
INNER_PALETTE = [
    "#10b981",  # In1.Cu: emerald
    "#f59e0b",  # In2.Cu: amber
    "#d946ef",  # In3.Cu: magenta
    "#06b6d4",  # In4.Cu: cyan
    "#f97316",  # In5.Cu: orange
    "#84cc16",  # In6.Cu: lime
    "#a855f7",  # In7.Cu: violet
    "#14b8a6",  # In8.Cu: teal
    "#eab308",  # In9.Cu: yellow
    "#ec4899",  # In10.Cu: pink
]

# DRC violation types that reflect silk, text or missing footprint libraries rather than routing.
NON_ROUTING_DRC = {
    "lib_footprint_issues",
    "silk_over_copper",
    "silk_overlap",
    "silk_edge_clearance",
    "text_height",
    "text_thickness",
    "nonmirrored_text_on_back_layer",
    "mirrored_text_on_front_layer",
    "courtyards_overlap",
    "malformed_courtyard",
}

KICAD_RULE_DEFAULTS = {
    "min_clearance": 0.0,
    "min_track_width": 0.0,
    "min_via_diameter": 0.5,
    "min_through_hole_diameter": 0.3,
    "min_via_annular_width": 0.1,
    "min_copper_edge_clearance": 0.5,
    "min_hole_clearance": 0.25,
    "min_hole_to_hole": 0.25,
    "solder_mask_to_copper_clearance": 0.0,
}

DEFAULT_NETCLASS = {
    "name": "Default",
    "clearance": 0.2,
    "track_width": 0.2,
    "via_diameter": 0.6,
    "via_drill": 0.3,
}


def layer_color(canon_name, inner_idx):
    if canon_name == "F.Cu":
        return "#ef4444"
    if canon_name == "B.Cu":
        return "#3b82f6"
    return INNER_PALETTE[inner_idx % len(INNER_PALETTE)]


def clip_poly_rect(pts, x0, y0, x1, y1):
    """Sutherland-Hodgman polygon clip against an axis-aligned box [x0, x1] x [y0, y1]."""
    if not pts:
        return []

    def clip_edge(poly, inside_fn, intersect_fn):
        if not poly:
            return []
        out = []
        prev = poly[-1]
        prev_in = inside_fn(prev)
        for cur in poly:
            cur_in = inside_fn(cur)
            if cur_in:
                if not prev_in:
                    out.append(intersect_fn(prev, cur))
                out.append(cur)
            elif prev_in:
                out.append(intersect_fn(prev, cur))
            prev, prev_in = cur, cur_in
        return out

    poly = pts
    poly = clip_edge(poly, lambda p: p[0] >= x0, lambda a, b: (x0, a[1] + (b[1] - a[1]) * (x0 - a[0]) / ((b[0] - a[0]) or 1e-12)))
    poly = clip_edge(poly, lambda p: p[0] <= x1, lambda a, b: (x1, a[1] + (b[1] - a[1]) * (x1 - a[0]) / ((b[0] - a[0]) or 1e-12)))
    poly = clip_edge(poly, lambda p: p[1] >= y0, lambda a, b: (a[0] + (b[0] - a[0]) * (y0 - a[1]) / ((b[1] - a[1]) or 1e-12), y0))
    poly = clip_edge(poly, lambda p: p[1] <= y1, lambda a, b: (a[0] + (b[0] - a[0]) * (y1 - a[1]) / ((b[1] - a[1]) or 1e-12), y1))
    return poly


def bbox_of_pts(pts):
    xs = [p[0] for p in pts]
    ys = [p[1] for p in pts]
    return (min(xs), min(ys), max(xs), max(ys))


def boxes_overlap(a, b):
    return a[0] <= b[2] and a[2] >= b[0] and a[1] <= b[3] and a[3] >= b[1]


def grow_box(box, m):
    return (box[0] - m, box[1] - m, box[2] + m, box[3] + m)


def extract_poly_set(ps):
    """Convert a pcbnew SHAPE_POLY_SET into a list of (bbox, outer_pts_mm, [hole_pts_mm, ...])."""
    res = []
    for i in range(ps.OutlineCount()):
        outline = ps.Outline(i)
        n = outline.PointCount()
        if n < 3:
            continue
        outer = [(outline.CPoint(k).x / MM, outline.CPoint(k).y / MM) for k in range(n)]
        bb = bbox_of_pts(outer)
        holes = []
        for h in range(ps.HoleCount(i)):
            hl = ps.Hole(i, h)
            hn = hl.PointCount()
            if hn >= 3:
                hpts = [(hl.CPoint(k).x / MM, hl.CPoint(k).y / MM) for k in range(hn)]
                holes.append((bbox_of_pts(hpts), hpts))
        res.append((bb, outer, holes))
    return res


def sample_arc_pts(cx, cy, sx, sy, angle_rad, steps=24):
    """Sample points along an arc from start point around centre by angle_rad."""
    r = math.hypot(sx - cx, sy - cy)
    a0 = math.atan2(sy - cy, sx - cx)
    n = max(6, int(abs(angle_rad) / (math.pi / 18)) + 1, steps)
    return [(cx + r * math.cos(a0 + angle_rad * k / n), cy + r * math.sin(a0 + angle_rad * k / n)) for k in range(n + 1)]


def arc_from_three_pts(p0, pm, p1, steps=24):
    """Sample a 3-point KiCad arc (start, mid, end) into polyline points in mm."""
    ax, ay = p0
    bx, by = pm
    cx, cy = p1
    if math.hypot(cx - ax, cy - ay) < 1e-6 and math.hypot(bx - ax, by - ay) > 1e-6:
        ux, uy = (ax + bx) * 0.5, (ay + by) * 0.5
        r = math.hypot(bx - ax, by - ay) * 0.5
        return [(ux + r * math.cos(2.0 * math.pi * k / 48), uy + r * math.sin(2.0 * math.pi * k / 48)) for k in range(49)]
    d = 2.0 * (ax * (by - cy) + bx * (cy - ay) + cx * (ay - by))
    if abs(d) < 1e-10:
        return [p0, p1]
    ux = ((ax * ax + ay * ay) * (by - cy) + (bx * bx + by * by) * (cy - ay) + (cx * cx + cy * cy) * (ay - by)) / d
    uy = ((ax * ax + ay * ay) * (cx - bx) + (bx * bx + by * by) * (ax - cx) + (cx * cx + cy * cy) * (bx - ax)) / d
    r = math.hypot(ax - ux, ay - uy)
    a0 = math.atan2(ay - uy, ax - ux)
    am = math.atan2(by - uy, bx - ux)
    a1 = math.atan2(cy - uy, cx - ux)

    def pos_mod(x):
        return x % (2.0 * math.pi)

    ccw_m = pos_mod(am - a0)
    ccw_1 = pos_mod(a1 - a0)
    sweep = ccw_1 if ccw_m <= ccw_1 else ccw_1 - 2.0 * math.pi
    n = max(8, int(abs(sweep) / (math.pi / 20)) + 1)
    return [(ux + r * math.cos(a0 + sweep * k / n), uy + r * math.sin(a0 + sweep * k / n)) for k in range(n + 1)]


def shape_to_polylines(g):
    """Extract stroked polylines in mm from a pcbnew PCB_SHAPE (for Edge.Cuts, Margin, CrtYd)."""
    if not isinstance(g, pcbnew.PCB_SHAPE):
        return []
    sh = g.GetShape()
    w = pcbnew.ToMM(g.GetWidth())
    if sh == pcbnew.SHAPE_T_SEGMENT:
        s, e = g.GetStart(), g.GetEnd()
        pts = [(s.x / MM, s.y / MM), (e.x / MM, e.y / MM)]
        return [(bbox_of_pts(pts), pts, w, False)]
    if sh in (getattr(pcbnew, "SHAPE_T_RECT", -1), getattr(pcbnew, "SHAPE_T_RECTANGLE", -2)):
        try:
            corners = [ (c.x / MM, c.y / MM) for c in g.GetRectCorners() ]
            pts = corners + [corners[0]]
            return [(bbox_of_pts(pts), pts, w, False)]
        except Exception:
            s, e = g.GetStart(), g.GetEnd()
            pts = [(s.x / MM, s.y / MM), (e.x / MM, s.y / MM), (e.x / MM, e.y / MM), (s.x / MM, e.y / MM), (s.x / MM, s.y / MM)]
            return [(bbox_of_pts(pts), pts, w, False)]
    if sh == pcbnew.SHAPE_T_CIRCLE:
        c = g.GetCenter()
        r = g.GetRadius() / MM
        cx, cy = c.x / MM, c.y / MM
        pts = [(cx + r * math.cos(2 * math.pi * k / 48), cy + r * math.sin(2 * math.pi * k / 48)) for k in range(49)]
        return [((cx - r, cy - r, cx + r, cy + r), pts, w, bool(g.IsFilled()))]
    if sh == pcbnew.SHAPE_T_ARC:
        s, m, e = g.GetStart(), g.GetArcMid(), g.GetEnd()
        pts = arc_from_three_pts((s.x / MM, s.y / MM), (m.x / MM, m.y / MM), (e.x / MM, e.y / MM))
        return [(bbox_of_pts(pts), pts, w, False)]
    # Polygon or bezier: ask pcbnew for its outline or polygonised form.
    ps = pcbnew.SHAPE_POLY_SET()
    g.TransformShapeToPolygon(ps, g.GetLayer(), 0, POLY_ERR, pcbnew.ERROR_INSIDE)
    out = []
    for bb, outer, _ in extract_poly_set(ps):
        out.append((bb, outer + [outer[0]], w, True))
    return out


def parse_rules(pro_path, dru_path, board_mask_margin_mm):
    """Read .kicad_pro and .kicad_dru beside the board to report design rules per net."""
    data = {}
    if pro_path and pro_path.exists():
        try:
            data = json.loads(pro_path.read_text())
        except Exception:
            data = {}
    r_raw = data.get("board", {}).get("design_settings", {}).get("rules", {}) or {}
    mins = {k: float(r_raw[k]) if r_raw.get(k) is not None else v for k, v in KICAD_RULE_DEFAULTS.items()}
    net_cfg = data.get("net_settings", {}) or {}
    classes = {}
    class_order = []
    for c in net_cfg.get("classes") or []:
        cname = c.get("name") or ""
        if not cname:
            continue
        class_order.append(cname)
        classes[cname] = {
            "name": cname,
            "priority": c.get("priority", 10**9),
            "clearance": c.get("clearance"),
            "track_width": c.get("track_width"),
            "via_diameter": c.get("via_diameter"),
            "via_drill": c.get("via_drill"),
        }
    if "Default" not in classes:
        classes["Default"] = dict(DEFAULT_NETCLASS, priority=10**9)
        class_order.append("Default")
    else:
        for k in ("clearance", "track_width", "via_diameter", "via_drill"):
            if classes["Default"].get(k) is None:
                classes["Default"][k] = DEFAULT_NETCLASS[k]

    assignments = {}
    raw_assign = net_cfg.get("netclass_assignments") or {}
    if isinstance(raw_assign, dict):
        for net, val in raw_assign.items():
            assignments[net] = val if isinstance(val, list) else [val]
    patterns = []
    for p in net_cfg.get("netclass_patterns") or []:
        if p.get("pattern") and p.get("netclass"):
            patterns.append((p["pattern"], p["netclass"]))

    dru_rules = []
    if dru_path and dru_path.exists():
        try:
            txt = dru_path.read_text()
            for m in re.finditer(r'\(rule\s+"([^"]+)"(.*?)\n\)', txt, re.S):
                rname = m.group(1)
                if rname == "pcbench neck":
                    continue
                dru_rules.append(rname)
        except Exception:
            pass

    def for_net(net_name):
        matched = []
        for cname in assignments.get(net_name, []):
            if cname in classes and cname != "Default" and cname not in matched:
                matched.append(cname)
        for pat, cname in patterns:
            if fnmatch.fnmatchcase(net_name, pat) and cname in classes and cname != "Default" and cname not in matched:
                matched.append(cname)
        matched.sort(key=lambda c: (classes[c]["priority"], class_order.index(c)))
        chain = [classes[c] for c in matched] + [classes["Default"]]

        def pick(field):
            for c in chain:
                if c.get(field) is not None:
                    return float(c[field])
            return DEFAULT_NETCLASS[field]

        clr = max(pick("clearance"), mins["min_clearance"])
        tw = max(pick("track_width"), mins["min_track_width"])
        vd = max(pick("via_diameter"), mins["min_via_diameter"])
        vdr = max(pick("via_drill"), mins["min_through_hole_diameter"])
        vd = max(vd, vdr + 2.0 * mins["min_via_annular_width"])
        return {
            "class": matched[0] if matched else "Default",
            "clearance": round(clr, 4),
            "track_width": round(tw, 4),
            "via_diameter": round(vd, 4),
            "via_drill": round(vdr, 4),
            "min_track_width": round(mins["min_track_width"], 4),
            "min_clearance": round(mins["min_clearance"], 4),
            "edge_clearance": round(mins["min_copper_edge_clearance"], 4),
            "hole_clearance": round(mins["min_hole_clearance"], 4),
            "mask_to_copper": round(mins["solder_mask_to_copper_clearance"], 4),
            "pad_mask_margin": round(board_mask_margin_mm, 4),
            "dru_rules": dru_rules,
        }

    return for_net


def poly_gap_approx(pts_a, pts_b):
    """Approximate minimum Euclidean edge/vertex distance in mm between two 2D polygons."""
    def seg_pt_dist2(ax, ay, bx, by, px, py):
        dx, dy = bx - ax, by - ay
        l2 = dx * dx + dy * dy
        if l2 <= 1e-18:
            return (px - ax) ** 2 + (py - ay) ** 2
        t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / l2))
        qx, qy = ax + t * dx, ay + t * dy
        return (px - qx) ** 2 + (py - qy) ** 2

    best2 = float("inf")
    na, nb = len(pts_a), len(pts_b)
    # Decimate dense circle approximations slightly if huge, keeping accuracy under 2 um.
    sa = pts_a if na <= 36 else pts_a[:: max(1, na // 32)]
    sb = pts_b if nb <= 36 else pts_b[:: max(1, nb // 32)]
    lsa, lsb = len(sa), len(sb)
    for i in range(lsa):
        ax, ay = sa[i]
        bx, by = sa[(i + 1) % lsa]
        for px, py in sb:
            d2 = seg_pt_dist2(ax, ay, bx, by, px, py)
            if d2 < best2:
                best2 = d2
    for i in range(lsb):
        ax, ay = sb[i]
        bx, by = sb[(i + 1) % lsb]
        for px, py in sa:
            d2 = seg_pt_dist2(ax, ay, bx, by, px, py)
            if d2 < best2:
                best2 = d2
    return math.sqrt(best2)


class BoardGeom:
    """All geometry of a loaded KiCad board in millimetres, indexed for fast multi-crop rendering."""

    def __init__(self, pcb_path, pro_path=None, dru_path=None):
        self.pcb_path = pcb_path
        b = pcbnew.LoadBoard(str(pcb_path))
        ds = b.GetDesignSettings()
        mask_margin_mm = pcbnew.ToMM(getattr(ds, "m_SolderMaskExpansion", 0))
        self.rules_for_net = parse_rules(pro_path, dru_path, mask_margin_mm)

        cu_ids = list(b.GetEnabledLayers().CuStack())
        if not cu_ids:
            cu_ids = [pcbnew.F_Cu, pcbnew.B_Cu]
        self.cu_ids = cu_ids
        self.cu_names = [pcbnew.LayerName(lid) for lid in cu_ids]
        self.lid_to_canon = {lid: pcbnew.LayerName(lid) for lid in cu_ids}
        # Map board-custom layer names (such as Front or Back) back to canonical names too.
        self.alias_to_canon = {}
        for lid in cu_ids:
            cn = pcbnew.LayerName(lid)
            self.alias_to_canon[cn] = cn
            self.alias_to_canon[b.GetLayerName(lid)] = cn

        self.layer_colors = {}
        inner_k = 0
        for cn in self.cu_names:
            self.layer_colors[cn] = layer_color(cn, inner_k)
            if cn not in ("F.Cu", "B.Cu"):
                inner_k += 1

        self.by_uuid = {}
        self.edges = []       # [(bbox, pts, width, filled)]
        self.courtyards = []  # [(bbox, pts)]
        self.fp_labels = []   # [(x, y, ref)]
        self.keepouts = []    # [{"bbox", "outer", "holes", "layers", " restrictions"}]
        self.mask_areas = []  # [{"bbox", "outer", "holes", "layer"}]
        self.zones = []       # [{"bbox", "outer", "holes", "layer", "net"}]
        self.graphics = []    # [{"bbox", "outer", "holes", "layer", "net"}]
        self.tracks = []      # [{"bbox", "pts", "w", "layer", "net", "uuid"}]
        self.vias = []        # [{"bbox", "x", "y", "r", "drill_r", "layers", "net", "uuid"}]
        self.pads = []        # [{"bbox", "x", "y", "ref", "num", "net", "kind", "layers", "polys", "mask_polys", "drill", "uuid"}]

        all_boxes = []

        # 1. Board and footprint drawings (Edge.Cuts, Margin, Courtyards, drawn copper).
        drawings = list(b.GetDrawings())
        for fp in b.Footprints():
            for g in fp.GraphicalItems():
                drawings.append(g)

        cu_set = set(cu_ids)
        crtyd_set = {pcbnew.F_CrtYd, pcbnew.B_CrtYd}
        edge_set = {pcbnew.Edge_Cuts, pcbnew.Margin}
        mask_map = {pcbnew.F_Mask: "F.Cu", pcbnew.B_Mask: "B.Cu"}

        for g in drawings:
            lid = g.GetLayer()
            gcls = g.GetClass()
            if lid in edge_set and isinstance(g, pcbnew.PCB_SHAPE):
                for bb, pts, w, filled in shape_to_polylines(g):
                    self.edges.append((bb, pts, w, filled))
                    all_boxes.append(bb)
            elif lid in crtyd_set and isinstance(g, pcbnew.PCB_SHAPE):
                for bb, pts, _, _ in shape_to_polylines(g):
                    self.courtyards.append((bb, pts))
            elif lid in mask_map:
                if gcls == "PCB_TEXT" and not g.IsVisible():
                    continue
                ps = pcbnew.SHAPE_POLY_SET()
                try:
                    if gcls == "PCB_TEXT" and hasattr(g, "TransformTextToPolySet") and not (hasattr(g, "IsKnockout") and g.IsKnockout()):
                        g.TransformTextToPolySet(ps, 0, POLY_ERR, pcbnew.ERROR_INSIDE)
                    else:
                        g.TransformShapeToPolygon(ps, lid, 0, POLY_ERR, pcbnew.ERROR_INSIDE)
                except Exception:
                    continue
                m_cu = mask_map[lid]
                for bb, outer, holes in extract_poly_set(ps):
                    self.mask_areas.append({"bbox": bb, "outer": outer, "holes": holes, "layer": m_cu})
            elif lid in cu_set:
                # Drawn copper graphic or copper text.
                if gcls == "PCB_TEXT" and not g.IsVisible():
                    continue
                ps = pcbnew.SHAPE_POLY_SET()
                try:
                    if gcls == "PCB_TEXT" and hasattr(g, "TransformTextToPolySet") and not (hasattr(g, "IsKnockout") and g.IsKnockout()):
                        g.TransformTextToPolySet(ps, 0, POLY_ERR, pcbnew.ERROR_INSIDE)
                    else:
                        g.TransformShapeToPolygon(ps, lid, 0, POLY_ERR, pcbnew.ERROR_INSIDE)
                except Exception:
                    continue
                net = g.GetNetname() if hasattr(g, "GetNetname") else ""
                cn = self.lid_to_canon[lid]
                for bb, outer, holes in extract_poly_set(ps):
                    self.graphics.append({"bbox": bb, "outer": outer, "holes": holes, "layer": cn, "net": net})
                    all_boxes.append(bb)

        self.has_outline = len(self.edges) > 0

        # 2. Zones and keepout rule areas (board and footprints).
        all_zones = list(b.Zones())
        for fp in b.Footprints():
            for z in fp.Zones():
                all_zones.append(z)

        for z in all_zones:
            uid = z.m_Uuid.AsString()
            z_layers = [self.lid_to_canon[lid] for lid in cu_ids if z.IsOnLayer(lid)]
            if z.GetIsRuleArea():
                forbid = []
                if z.GetDoNotAllowTracks():
                    forbid.append("tracks")
                if z.GetDoNotAllowVias():
                    forbid.append("vias")
                if z.GetDoNotAllowPads():
                    forbid.append("pads")
                if z.GetDoNotAllowCopperPour():
                    forbid.append("pours")
                for bb, outer, holes in extract_poly_set(z.Outline()):
                    ko = {"bbox": bb, "outer": outer, "holes": holes, "layers": set(z_layers), "forbid": forbid, "uuid": uid}
                    self.keepouts.append(ko)
                    all_boxes.append(bb)
            else:
                net = z.GetNetname()
                any_fill = False
                for lid in cu_ids:
                    if not z.IsOnLayer(lid):
                        continue
                    cn = self.lid_to_canon[lid]
                    fps = z.GetFilledPolysList(lid)
                    for bb, outer, holes in extract_poly_set(fps):
                        any_fill = True
                        zf = {"bbox": bb, "outer": outer, "holes": holes, "layer": cn, "net": net, "filled": True, "uuid": uid}
                        self.zones.append(zf)
                if not any_fill:
                    for lid in cu_ids:
                        if z.IsOnLayer(lid):
                            cn = self.lid_to_canon[lid]
                            for bb, outer, holes in extract_poly_set(z.Outline()):
                                self.zones.append({"bbox": bb, "outer": outer, "holes": holes, "layer": cn, "net": net, "filled": False, "uuid": uid})

        # 3. Pads and footprints.
        for fp in b.Footprints():
            ref = fp.GetReference() or ""
            fp_pts = []
            for p in fp.Pads():
                uid = p.m_Uuid.AsString()
                pos = p.GetPosition()
                px, py = pos.x / MM, pos.y / MM
                fp_pts.append((px, py))
                num = p.GetNumber() or ""
                net = p.GetNetname() or ""
                attr = p.GetAttribute()
                p_layers = [self.lid_to_canon[lid] for lid in cu_ids if p.IsOnLayer(lid)]

                if attr == pcbnew.PAD_ATTRIB_NPTH:
                    kind = "npth"
                elif attr == pcbnew.PAD_ATTRIB_PTH or len(p_layers) > 1:
                    kind = "pth"
                elif p_layers == ["B.Cu"]:
                    kind = "smd_back"
                elif p_layers == ["F.Cu"]:
                    kind = "smd_front"
                else:
                    kind = "smd_other"

                polys = {}
                p_boxes = []
                for lid in cu_ids:
                    if p.IsOnLayer(lid):
                        cn = self.lid_to_canon[lid]
                        ep = p.GetEffectivePolygon(lid)
                        extracted = extract_poly_set(ep)
                        if extracted:
                            polys[cn] = extracted
                            for bb, _, _ in extracted:
                                p_boxes.append(bb)

                mask_polys = {}
                for mlid, mcn in ((pcbnew.F_Mask, "F.Cu"), (pcbnew.B_Mask, "B.Cu")):
                    if p.IsOnLayer(mlid) and mcn in polys:
                        mp = p.GetEffectivePolygon(mlid)
                        mext = extract_poly_set(mp)
                        if mext:
                            # Keep mask outline only when it noticeably expands beyond pad copper.
                            mb = mext[0][0]
                            cb = polys[mcn][0][0]
                            if (mb[2] - mb[0]) - (cb[2] - cb[0]) > 0.01 or (mb[3] - mb[1]) - (cb[3] - cb[1]) > 0.01:
                                mask_polys[mcn] = mext

                drill = None
                dsz = p.GetDrillSize()
                if dsz.x > 0 and dsz.y > 0:
                    dx, dy = dsz.x / MM, dsz.y / MM
                    # Note: in KiCad a pad's position is its drill centre; copper may be offset.
                    angle_deg = p.GetOrientation().AsDegrees()
                    drill = {"x": px, "y": py, "w": dx, "h": dy, "oblong": p.GetDrillShape() == pcbnew.PAD_DRILL_SHAPE_OBLONG, "angle": angle_deg}
                    r_max = 0.5 * max(dx, dy)
                    p_boxes.append((px - r_max, py - r_max, px + r_max, py + r_max))

                if p_boxes:
                    bb = (min(x[0] for x in p_boxes), min(x[1] for x in p_boxes), max(x[2] for x in p_boxes), max(x[3] for x in p_boxes))
                else:
                    bb = (px - 0.1, py - 0.1, px + 0.1, py + 0.1)

                # Representative copper polygon for gap/pitch calculations.
                rep_poly = None
                for cn in self.cu_names:
                    if cn in polys and polys[cn]:
                        rep_poly = polys[cn][0][1]
                        break

                pad_rec = {
                    "kind": kind,
                    "item_type": "pad",
                    "bbox": bb,
                    "x": px,
                    "y": py,
                    "ref": ref,
                    "num": num,
                    "label": f"{ref}.{num}" if num else ref,
                    "net": net,
                    "layers": p_layers,
                    "polys": polys,
                    "rep_poly": rep_poly,
                    "mask_polys": mask_polys,
                    "drill": drill,
                    "uuid": uid,
                }
                self.pads.append(pad_rec)
                self.by_uuid[uid] = pad_rec
                all_boxes.append(bb)

            if ref and not ref.startswith(("REF**", "Noname", "LOGO")):
                if fp_pts:
                    fx = sum(p[0] for p in fp_pts) / len(fp_pts)
                    fy = sum(p[1] for p in fp_pts) / len(fp_pts)
                else:
                    fpos = fp.GetPosition()
                    fx, fy = fpos.x / MM, fpos.y / MM
                self.fp_labels.append((fx, fy, ref))

        # 4. Tracks, arcs, and vias.
        for t in b.GetTracks():
            uid = t.m_Uuid.AsString()
            net = t.GetNetname() or ""
            cls = t.GetClass()
            if cls == "PCB_VIA":
                pos = t.GetPosition()
                vx, vy = pos.x / MM, pos.y / MM
                r = 0.5 * (t.GetWidth(cu_ids[0]) / MM)
                dr = 0.5 * (t.GetDrillValue() / MM)
                top_cn = self.lid_to_canon.get(t.TopLayer(), self.cu_names[0])
                bot_cn = self.lid_to_canon.get(t.BottomLayer(), self.cu_names[-1])
                i0 = self.cu_names.index(top_cn) if top_cn in self.cu_names else 0
                i1 = self.cu_names.index(bot_cn) if bot_cn in self.cu_names else len(self.cu_names) - 1
                if i0 > i1:
                    i0, i1 = i1, i0
                v_layers = self.cu_names[i0 : i1 + 1]
                bb = (vx - r, vy - r, vx + r, vy + r)
                vrec = {"item_type": "via", "bbox": bb, "x": vx, "y": vy, "r": r, "drill_r": dr, "layers": v_layers, "net": net, "uuid": uid}
                self.vias.append(vrec)
                self.by_uuid[uid] = vrec
                all_boxes.append(bb)
            elif cls == "PCB_ARC":
                lid = t.GetLayer()
                cn = self.lid_to_canon.get(lid, self.cu_names[0])
                w = t.GetWidth() / MM
                s, m, e = t.GetStart(), t.GetMid(), t.GetEnd()
                pts = arc_from_three_pts((s.x / MM, s.y / MM), (m.x / MM, m.y / MM), (e.x / MM, e.y / MM))
                bb = grow_box(bbox_of_pts(pts), 0.5 * w)
                trec = {"item_type": "arc", "bbox": bb, "pts": pts, "w": w, "layer": cn, "net": net, "uuid": uid}
                self.tracks.append(trec)
                self.by_uuid[uid] = trec
                all_boxes.append(bb)
            else:
                lid = t.GetLayer()
                cn = self.lid_to_canon.get(lid, self.cu_names[0])
                w = t.GetWidth() / MM
                s, e = t.GetStart(), t.GetEnd()
                pts = [(s.x / MM, s.y / MM), (e.x / MM, e.y / MM)]
                bb = grow_box(bbox_of_pts(pts), 0.5 * w)
                trec = {"item_type": "track", "bbox": bb, "pts": pts, "w": w, "layer": cn, "net": net, "uuid": uid}
                self.tracks.append(trec)
                self.by_uuid[uid] = trec
                all_boxes.append(bb)

        # Board bounding box: prefer Edge.Cuts if present and non-degenerate, else all items.
        edge_boxes = [e[0] for e in self.edges]
        if edge_boxes:
            eb = (min(x[0] for x in edge_boxes), min(x[1] for x in edge_boxes), max(x[2] for x in edge_boxes), max(x[3] for x in edge_boxes))
            # Include any pads/tracks straying outside an incomplete Edge.Cuts fragment.
            if all_boxes:
                ab = (min(x[0] for x in all_boxes), min(x[1] for x in all_boxes), max(x[2] for x in all_boxes), max(x[3] for x in all_boxes))
                if (eb[2] - eb[0]) < 1.0 or (eb[3] - eb[1]) < 1.0:
                    self.board_bbox = grow_box(ab, 1.5)
                else:
                    self.board_bbox = (min(eb[0], ab[0]), min(eb[1], ab[1]), max(eb[2], ab[2]), max(eb[3], ab[3]))
            else:
                self.board_bbox = eb
        elif all_boxes:
            self.board_bbox = grow_box((min(x[0] for x in all_boxes), min(x[1] for x in all_boxes), max(x[2] for x in all_boxes), max(x[3] for x in all_boxes)), 1.5)
        else:
            self.board_bbox = (0.0, 0.0, 50.0, 50.0)

    def pitch_near_point(self, x, y, uid=None, net=None):
        """Return nearest neighbour pad pitch and copper edge-to-edge gap near (x, y)."""
        anchor_pad = self.by_uuid.get(uid) if uid else None
        if not anchor_pad or anchor_pad.get("item_type") != "pad":
            # Find closest pad of the same net near (x, y) within 2.5 mm, else any pad within 1.5 mm.
            best_d = 2.5
            anchor_pad = None
            for p in self.pads:
                if net and p["net"] != net:
                    continue
                d = math.hypot(p["x"] - x, p["y"] - y)
                if d < best_d:
                    best_d = d
                    anchor_pad = p
        if not anchor_pad:
            return None

        pset = set(anchor_pad["layers"])
        best_pitch = None
        best_gap = None
        best_other = None
        for q in self.pads:
            if q is anchor_pad:
                continue
            if pset and q["layers"] and not (pset & set(q["layers"])):
                continue
            c_dist = math.hypot(q["x"] - anchor_pad["x"], q["y"] - anchor_pad["y"])
            if c_dist > 8.0 or c_dist < 1e-4:
                continue
            if best_pitch is None or c_dist < best_pitch:
                best_pitch = c_dist
                best_other = q
                if anchor_pad["rep_poly"] and q["rep_poly"]:
                    best_gap = poly_gap_approx(anchor_pad["rep_poly"], q["rep_poly"])
        if best_pitch is None:
            return {"pad": anchor_pad["label"], "pitch_mm": None, "gap_mm": None}
        return {
            "pad": anchor_pad["label"],
            "neighbour": best_other["label"] if best_other else "",
            "pitch_mm": round(best_pitch, 3),
            "gap_mm": round(best_gap, 3) if best_gap is not None else None,
        }


def parse_drc_endpoint(item_dict, geom):
    """Extract structured endpoint details from an unconnected_items entry in out.drc.json."""
    desc = item_dict.get("description", "")
    pos = item_dict.get("pos") or {}
    x, y = float(pos.get("x", 0.0)), float(pos.get("y", 0.0))
    uid = item_dict.get("uuid", "")
    obj = geom.by_uuid.get(uid)

    m_pad = re.match(r"^(?:(PTH|NPTH)\s+)?[Pp]ad(?:\s+([^\[]*?))?\s+\[(.*)\]\s+of\s+(\S+)(?:\s+on\s+(.+))?$", desc)
    if m_pad:
        kind_tag, pin_num, net, ref, layer_str = m_pad.groups()
        pin_num = (pin_num or "").strip()
        ref_pin = f"{ref}.{pin_num}" if pin_num else ref
        canon_layer = geom.alias_to_canon.get(layer_str, layer_str) if layer_str else ("PTH" if kind_tag == "PTH" else "")
        return {
            "kind": "pad",
            "ref": ref,
            "pin": pin_num,
            "ref_pin": ref_pin,
            "report_key": f"{ref}-{pin_num}" if pin_num else ref,
            "net": net,
            "layer": canon_layer or ("/".join(obj["layers"]) if obj and obj.get("layers") else ""),
            "x": x,
            "y": y,
            "uuid": uid,
            "raw": desc,
        }

    m_other = re.match(r"^(\S+)\s+\[(.*)\](?:\s+on\s+(\S+?))?(?:,|\s*$)(.*)$", desc)
    if m_other:
        kind_word, net, layer_str, _ = m_other.groups()
        canon_layer = geom.alias_to_canon.get(layer_str, layer_str) if layer_str else ""
        # Check if a pad of the same net sits very close or shares copper so ref.pin context is clear.
        near_pad = None
        best_d = 1.8
        for p in geom.pads:
            if p["net"] == net:
                d = math.hypot(p["x"] - x, p["y"] - y)
                if d < best_d:
                    best_d = d
                    near_pad = p["label"]
        label = kind_word.lower()
        if near_pad:
            disp = f"{kind_word} (near {near_pad})"
        else:
            disp = kind_word
        return {
            "kind": label,
            "ref": "",
            "pin": "",
            "ref_pin": disp,
            "report_key": f"a {label}",
            "net": net,
            "layer": canon_layer,
            "x": x,
            "y": y,
            "uuid": uid,
            "raw": desc,
        }

    return {
        "kind": "copper",
        "ref": "",
        "pin": "",
        "ref_pin": desc[:28],
        "report_key": "copper",
        "net": obj.get("net", "") if obj else "",
        "layer": "",
        "x": x,
        "y": y,
        "uuid": uid,
        "raw": desc,
    }


def match_report_failures(unconn_parsed, report_data):
    """Match each DRC unconnected item to the router's --report stage, reason and findings."""
    if not report_data:
        return [{"stage": None, "reason": None, "display": "no --report provided", "mismatch": False, "rule_warnings": []}] * len(unconn_parsed)

    rule_warnings = [f["message"] for f in report_data.get("findings", []) if f.get("kind") in ("rule", "board")]
    open_findings = [f for f in report_data.get("findings", []) if f.get("kind") == "open"]
    failures = report_data.get("failures") or []
    router_opens_total = report_data.get("summary", {}).get("opens", 0)

    open_by_net = defaultdict(list)
    for f in open_findings:
        for n in f.get("nets") or []:
            open_by_net[n].append(f)

    fail_by_net = defaultdict(list)
    for fl in failures:
        fail_by_net[fl.get("net", "")].append(fl)

    def fmt_end(s):
        # Turn router's "U1-4" into "U1.4" for consistent display.
        if s in ("a track", "a via", "copper", "gone"):
            return s
        if "-" in s:
            r, p = s.rsplit("-", 1)
            return f"{r}.{p}"
        return s

    used_fails = set()
    results = []
    for a, b in unconn_parsed:
        net = a["net"] or b["net"]
        net_opens = open_by_net.get(net, [])
        mismatch = (router_opens_total == 0) or (len(net_opens) == 0)

        keys = {a["report_key"], b["report_key"]}
        candidates = fail_by_net.get(net, [])
        best_idx = None
        best_score = -1
        for idx, fl in enumerate(candidates):
            f_ends = {fl.get("from", ""), fl.get("to", "")}
            matched = keys & f_ends
            pad_hits = sum(1 for k in matched if k not in ("a track", "a via", "copper", "gone"))
            gen_hits = len(matched) - pad_hits
            # Prefer concrete ref-pin pad matches over generic "a track" matches, then unused entries.
            score = pad_hits * 30 + gen_hits * 6 + (0 if idx in used_fails else 3) + idx * 0.01
            if score > best_score:
                best_score = score
                best_idx = idx

        if mismatch:
            stage = "result (mismatch)"
            reason = "router says connected (0 open on net), KiCad DRC says open"
            disp = f"MISMATCH: {reason}"
        elif best_idx is not None:
            used_fails.add(best_idx)
            fl = candidates[best_idx]
            stage = fl["stage"]
            raw_reason = fl["reason"]
            fr, to = fmt_end(fl["from"]), fmt_end(fl["to"])
            if raw_reason == "no terminal at the start":
                reason = f"no terminal at the start ({fr})"
            elif raw_reason == "no terminal at the end":
                reason = f"no terminal at the end ({to})"
            else:
                reason = f"{raw_reason} ({fr} -> {to})"
            disp = f"{stage}: {reason}"
        else:
            # Open in findings, but not in finishing's failure list (e.g. knocked open or pairs/planes).
            stage = "post-finishing"
            reason = "left open at result without a finishing failure entry"
            disp = f"{stage}: {reason}"

        results.append({
            "stage": stage,
            "reason": reason,
            "raw_reason": candidates[best_idx]["reason"] if (not mismatch and best_idx is not None) else None,
            "display": disp,
            "mismatch": mismatch,
            "router_opens": router_opens_total,
            "rule_warnings": rule_warnings,
        })
    return results


def relevant_drc_violations(drc_data, alias_to_canon=None):
    """Extract copper, mask, hole and edge DRC violations for drawing markers on crops."""
    alias_to_canon = alias_to_canon or {}
    out = []
    for v in drc_data.get("violations", []):
        vtype = v.get("type", "")
        sev = v.get("severity", "warning")
        if vtype in NON_ROUTING_DRC:
            continue
        desc = v.get("description", "")
        is_neck = vtype == "connection_width" and "pcbench neck" in desc
        items = v.get("items") or []
        involves_routed = vtype == "starved_thermal" or is_neck or any(
            re.match(r"^(Track|Via|Arc)\s+\[", it.get("description", "")) for it in items
        )
        v_layers = set()
        if "Front solder mask" in desc:
            v_layers.add("F.Cu")
        elif "Rear solder mask" in desc:
            v_layers.add("B.Cu")
        for it in items:
            idesc = it.get("description", "")
            m_on = re.search(r"\bon\s+([A-Za-z0-9_.-]+)", idesc)
            if m_on:
                lname = m_on.group(1).rstrip(",")
                v_layers.add(alias_to_canon.get(lname, lname))
        pts = []
        for it in items:
            p = it.get("pos")
            if p and "x" in p and "y" in p:
                pts.append((float(p["x"]), float(p["y"])))
        if not pts:
            continue
        mx = sum(p[0] for p in pts) / len(pts)
        my = sum(p[1] for p in pts) / len(pts)
        out.append({
            "type": "neck" if is_neck else vtype,
            "severity": sev,
            "routed": involves_routed,
            "layers": v_layers,
            "x": mx,
            "y": my,
            "pts": pts,
            "desc": desc,
        })
    return out


def render_panel_svg(
    geom,
    vbox,
    px_box,
    clip_id,
    open_net=None,
    only_layer=None,
    endpoints=None,
    all_airwires=None,
    violations=None,
    show_pin_labels=True,
    show_mask_halos=False,
    panel_title=None,
):
    """Render one PCB viewport panel into SVG elements mapped onto pixel rect px_box = (px, py, pw, ph)."""
    vx0, vy0, vx1, vy1 = vbox
    px, py, pw, ph = px_box
    vw = max(vx1 - vx0, 1e-6)
    vh = max(vy1 - vy0, 1e-6)
    scale = min(pw / vw, ph / vh)
    used_w = vw * scale
    used_h = vh * scale
    ox = px + 0.5 * (pw - used_w)
    oy = py + 0.5 * (ph - used_h)

    def sx(x):
        return ox + (x - vx0) * scale

    def sy(y):
        return oy + (y - vy0) * scale

    def sr(mm_val):
        return mm_val * scale

    # Clipping box in board mm with small margin for wide strokes.
    cbox = grow_box((vx0 - (ox - px) / scale, vy0 - (oy - py) / scale, vx1 + (ox - px) / scale, vy1 + (oy - py) / scale), 1.0)
    clip_margin = grow_box(cbox, 0.5)

    lines = []
    lines.append(f'<defs><clipPath id="{clip_id}"><rect x="{px:.1f}" y="{py:.1f}" width="{pw:.1f}" height="{ph:.1f}" rx="6"/></clipPath></defs>')
    lines.append(f'<rect x="{px:.1f}" y="{py:.1f}" width="{pw:.1f}" height="{ph:.1f}" rx="6" fill="#12161f" stroke="#2c3444" stroke-width="1"/>')
    lines.append(f'<g clip-path="url(#{clip_id})">')

    # Subtle grid dots/lines at 1 mm or 5 mm intervals depending on zoom.
    span_mm = max(vw, vh)
    grid_step = 0.5 if span_mm <= 6 else 1.0 if span_mm <= 18 else 5.0 if span_mm <= 80 else 10.0
    gx0 = math.floor(cbox[0] / grid_step) * grid_step
    gy0 = math.floor(cbox[1] / grid_step) * grid_step
    gpaths = []
    gx = gx0
    while gx <= cbox[2]:
        X = sx(gx)
        gpaths.append(f"M{X:.1f},{py:.1f}V{py + ph:.1f}")
        gx += grid_step
    gy = gy0
    while gy <= cbox[3]:
        Y = sy(gy)
        gpaths.append(f"M{px:.1f},{Y:.1f}H{px + pw:.1f}")
        gy += grid_step
    if len(gpaths) <= 160:
        lines.append(f'<path d="{" ".join(gpaths)}" stroke="#1c2230" stroke-width="0.7"/>')

    def poly_path_d(outer, holes):
        clipped = clip_poly_rect(outer, clip_margin[0], clip_margin[1], clip_margin[2], clip_margin[3])
        if len(clipped) < 3:
            return ""
        parts = ["M" + "L".join(f"{sx(p[0]):.2f},{sy(p[1]):.2f}" for p in clipped) + "Z"]
        for hbb, hpts in holes:
            if boxes_overlap(hbb, cbox):
                hc = clip_poly_rect(hpts, clip_margin[0], clip_margin[1], clip_margin[2], clip_margin[3])
                if len(hc) >= 3:
                    parts.append("M" + "L".join(f"{sx(p[0]):.2f},{sy(p[1]):.2f}" for p in hc) + "Z")
        return " ".join(parts)

    # 1. Filled / unfilled copper zones (faint).
    for z in geom.zones:
        if only_layer and z["layer"] != only_layer:
            continue
        if not boxes_overlap(z["bbox"], cbox):
            continue
        col = geom.layer_colors.get(z["layer"], "#64748b")
        is_open = open_net and z["net"] == open_net
        d = poly_path_d(z["outer"], z["holes"])
        if not d:
            continue
        if z["filled"]:
            op = 0.28 if is_open else 0.13
            st = f' stroke="{col}" stroke-width="1.2" stroke-opacity="0.7"' if is_open else ""
            lines.append(f'<path d="{d}" fill="{col}" fill-opacity="{op}" fill-rule="evenodd"{st}/>')
        else:
            lines.append(f'<path d="{d}" fill="none" stroke="{col}" stroke-width="1" stroke-dasharray="4,3" stroke-opacity="0.45"/>')

    # 2. Keepout rule areas (hatched magenta-rose).
    for ko in geom.keepouts:
        if only_layer and ko["layers"] and only_layer not in ko["layers"]:
            continue
        if not boxes_overlap(ko["bbox"], cbox):
            continue
        d = poly_path_d(ko["outer"], ko["holes"])
        if d:
            lines.append(f'<path d="{d}" fill="url(#keepoutHatch)" stroke="#fb7185" stroke-width="1.4" stroke-dasharray="5,3" fill-rule="evenodd"/>')
            cx_mm = 0.5 * (max(ko["bbox"][0], vx0) + min(ko["bbox"][2], vx1))
            cy_mm = 0.5 * (max(ko["bbox"][1], vy0) + min(ko["bbox"][3], vy1))
            tag = "KEEPOUT: no " + "/".join(ko["forbid"] or ["copper"])
            lines.append(f'<text x="{sx(cx_mm):.1f}" y="{sy(cy_mm):.1f}" fill="#fda4af" font-size="10" font-weight="600" text-anchor="middle">{xml_esc(tag)}</text>')

    # 2b. Drawn solder mask openings on F.Mask / B.Mask (violet dashed, blocks foreign copper).
    for ma in geom.mask_areas:
        if only_layer and ma["layer"] != only_layer:
            continue
        if not boxes_overlap(ma["bbox"], cbox):
            continue
        d = poly_path_d(ma["outer"], ma["holes"])
        if d:
            lines.append(f'<path d="{d}" fill="#c084fc" fill-opacity="0.14" stroke="#c084fc" stroke-width="1.2" stroke-dasharray="3,2" fill-rule="evenodd"/>')

    # 3. Courtyards (faint).
    for bb, pts in geom.courtyards:
        if not boxes_overlap(bb, cbox):
            continue
        p_str = " ".join(f"{sx(p[0]):.2f},{sy(p[1]):.2f}" for p in pts)
        lines.append(f'<polyline points="{p_str}" fill="none" stroke="#64748b" stroke-width="0.8" stroke-opacity="0.32" stroke-dasharray="3,2"/>')

    # 4. Drawn copper graphics on copper layers.
    for g in geom.graphics:
        if only_layer and g["layer"] != only_layer:
            continue
        if not boxes_overlap(g["bbox"], cbox):
            continue
        d = poly_path_d(g["outer"], g["holes"])
        if not d:
            continue
        col = geom.layer_colors.get(g["layer"], "#94a3b8")
        is_open = open_net and g["net"] == open_net
        if is_open:
            lines.append(f'<path d="{d}" fill="{col}" fill-opacity="0.88" stroke="#fde047" stroke-width="1.8" fill-rule="evenodd"/>')
        else:
            lines.append(f'<path d="{d}" fill="{col}" fill-opacity="0.62" fill-rule="evenodd"/>')

    # 5. Tracks and arcs: draw other nets first (inner layers, then B.Cu, then F.Cu), then open net outlined on top.
    layer_draw_order = [cn for cn in geom.cu_names if cn not in ("F.Cu", "B.Cu")]
    if "B.Cu" in geom.cu_names:
        layer_draw_order.append("B.Cu")
    if "F.Cu" in geom.cu_names:
        layer_draw_order.append("F.Cu")
    layer_rank = {cn: idx for idx, cn in enumerate(layer_draw_order)}

    vis_tracks = [
        t for t in geom.tracks
        if (not only_layer or t["layer"] == only_layer) and boxes_overlap(t["bbox"], cbox)
    ]
    vis_tracks.sort(key=lambda t: (1 if (open_net and t["net"] == open_net) else 0, layer_rank.get(t["layer"], 0)))

    for t in vis_tracks:
        col = geom.layer_colors.get(t["layer"], "#94a3b8")
        sw = max(sr(t["w"]), 1.1)
        p_str = " ".join(f"{sx(p[0]):.2f},{sy(p[1]):.2f}" for p in t["pts"])
        is_open = open_net and t["net"] == open_net
        if is_open:
            lines.append(f'<polyline points="{p_str}" fill="none" stroke="#fef08a" stroke-width="{sw + 3.0:.2f}" stroke-linecap="round" stroke-linejoin="round" stroke-opacity="0.95"/>')
            lines.append(f'<polyline points="{p_str}" fill="none" stroke="{col}" stroke-width="{sw:.2f}" stroke-linecap="round" stroke-linejoin="round"/>')
        else:
            lines.append(f'<polyline points="{p_str}" fill="none" stroke="{col}" stroke-width="{sw:.2f}" stroke-linecap="round" stroke-linejoin="round" stroke-opacity="0.72"/>')

    # 6. Pads (other nets first, then open net strongly highlighted).
    vis_pads = [
        p for p in geom.pads
        if boxes_overlap(p["bbox"], cbox) and (not only_layer or only_layer in p["layers"] or p["kind"] == "npth")
    ]
    vis_pads.sort(key=lambda p: 1 if (open_net and p["net"] == open_net) else 0)

    for p in vis_pads:
        is_open = bool(open_net and p["net"] == open_net)
        kind = p["kind"]
        if kind == "npth":
            dr = p["drill"]
            if dr:
                cx_p, cy_p = sx(dr["x"]), sy(dr["y"])
                if dr["oblong"]:
                    rw, rh = max(sr(dr["w"]), 2.0), max(sr(dr["h"]), 2.0)
                    lines.append(f'<rect x="{cx_p - 0.5*rw:.2f}" y="{cy_p - 0.5*rh:.2f}" width="{rw:.2f}" height="{rh:.2f}" rx="{0.5*min(rw,rh):.2f}" transform="rotate({-dr["angle"]:.1f} {cx_p:.2f} {cy_p:.2f})" fill="#090b10" stroke="#94a3b8" stroke-width="1.2" stroke-dasharray="3,2"/>')
                else:
                    rr = max(0.5 * sr(dr["w"]), 1.5)
                    lines.append(f'<circle cx="{cx_p:.2f}" cy="{cy_p:.2f}" r="{rr:.2f}" fill="#090b10" stroke="#94a3b8" stroke-width="1.2" stroke-dasharray="3,2"/>')
            continue

        if only_layer and only_layer in p["polys"]:
            poly_list = p["polys"][only_layer]
            fill_col = geom.layer_colors.get(only_layer, "#ca8a04") if kind != "pth" else "#ca8a04"
            stroke_col = "#fde047" if kind == "pth" else fill_col
        else:
            # Pick representative copper polygon.
            first_cn = next((cn for cn in ("F.Cu", "B.Cu", *geom.cu_names) if cn in p["polys"]), None)
            poly_list = p["polys"].get(first_cn, []) if first_cn else []
            if kind == "pth":
                fill_col, stroke_col = "#ca8a04", "#fde047"
            elif kind == "smd_front":
                fill_col, stroke_col = "#dc2626", "#fca5a5"
            elif kind == "smd_back":
                fill_col, stroke_col = "#2563eb", "#93c5fd"
            else:
                c = geom.layer_colors.get(first_cn, "#10b981")
                fill_col, stroke_col = c, "#e2e8f0"

        if show_mask_halos and span_mm <= 16:
            for mcn, mext in p["mask_polys"].items():
                if only_layer and mcn != only_layer:
                    continue
                for _, mouter, mholes in mext:
                    md = poly_path_d(mouter, mholes)
                    if md:
                        lines.append(f'<path d="{md}" fill="none" stroke="#fb923c" stroke-width="0.8" stroke-dasharray="2,2" stroke-opacity="0.55"/>')

        for _, outer, holes in poly_list:
            d = poly_path_d(outer, holes)
            if not d:
                continue
            if is_open:
                lines.append(f'<path d="{d}" fill="#bef264" fill-opacity="0.95" stroke="#ffffff" stroke-width="2.0" fill-rule="evenodd"/>')
            else:
                lines.append(f'<path d="{d}" fill="{fill_col}" fill-opacity="0.78" stroke="{stroke_col}" stroke-width="0.8" stroke-opacity="0.85" fill-rule="evenodd"/>')

        dr = p["drill"]
        if dr:
            cx_p, cy_p = sx(dr["x"]), sy(dr["y"])
            if dr["oblong"]:
                rw, rh = max(sr(dr["w"]), 1.8), max(sr(dr["h"]), 1.8)
                lines.append(f'<rect x="{cx_p - 0.5*rw:.2f}" y="{cy_p - 0.5*rh:.2f}" width="{rw:.2f}" height="{rh:.2f}" rx="{0.5*min(rw,rh):.2f}" transform="rotate({-dr["angle"]:.1f} {cx_p:.2f} {cy_p:.2f})" fill="#090c12" stroke="#475569" stroke-width="0.8"/>')
            else:
                rr = max(0.5 * sr(dr["w"]), 1.2)
                lines.append(f'<circle cx="{cx_p:.2f}" cy="{cy_p:.2f}" r="{rr:.2f}" fill="#090c12" stroke="#475569" stroke-width="0.8"/>')

        if show_pin_labels and (is_open or span_mm <= 10.0):
            pw_px = (p["bbox"][2] - p["bbox"][0]) * scale
            ph_px = (p["bbox"][3] - p["bbox"][1]) * scale
            if max(pw_px, ph_px) >= 14 and p["num"]:
                fs = max(7.5, min(10.5, 0.38 * min(max(pw_px, 12), max(ph_px, 12))))
                tcol = "#0f172a" if is_open else "#f8fafc"
                lines.append(f'<text x="{sx(p["x"]):.1f}" y="{sy(p["y"]) + 0.34*fs:.1f}" fill="{tcol}" font-size="{fs:.1f}" font-weight="700" text-anchor="middle">{xml_esc(p["num"])}</text>')

    # 7. Vias.
    for v in geom.vias:
        if only_layer and only_layer not in v["layers"]:
            continue
        if not boxes_overlap(v["bbox"], cbox):
            continue
        cx_v, cy_v = sx(v["x"]), sy(v["y"])
        vr = max(sr(v["r"]), 2.2)
        dr = max(sr(v["drill_r"]), 1.0)
        is_open = open_net and v["net"] == open_net
        if is_open:
            lines.append(f'<circle cx="{cx_v:.2f}" cy="{cy_v:.2f}" r="{vr + 2.0:.2f}" fill="none" stroke="#fef08a" stroke-width="2.2"/>')
            lines.append(f'<circle cx="{cx_v:.2f}" cy="{cy_v:.2f}" r="{vr:.2f}" fill="#bef264" stroke="#ffffff" stroke-width="1.0"/>')
        else:
            lines.append(f'<circle cx="{cx_v:.2f}" cy="{cy_v:.2f}" r="{vr:.2f}" fill="#cbd5e1" fill-opacity="0.88" stroke="#475569" stroke-width="0.8"/>')
        lines.append(f'<circle cx="{cx_v:.2f}" cy="{cy_v:.2f}" r="{dr:.2f}" fill="#090c12"/>')

    # 8. Board outline and cutouts (Edge.Cuts / Margin) on top so edges/cutouts are unmistakable.
    for bb, pts, _, _ in geom.edges:
        if not boxes_overlap(bb, cbox):
            continue
        p_str = " ".join(f"{sx(p[0]):.2f},{sy(p[1]):.2f}" for p in pts)
        lines.append(f'<polyline points="{p_str}" fill="none" stroke="#facc15" stroke-width="2.0" stroke-linecap="round" stroke-linejoin="round"/>')

    # 9. Footprint reference labels (subtle).
    if span_mm <= 45:
        for fx, fy, ref in geom.fp_labels:
            if cbox[0] <= fx <= cbox[2] and cbox[1] <= fy <= cbox[3]:
                lines.append(f'<text x="{sx(fx):.1f}" y="{sy(fy):.1f}" fill="#94a3b8" fill-opacity="0.65" font-size="9.5" font-weight="600" text-anchor="middle">{xml_esc(ref)}</text>')

    # 10. DRC violation markers in the crop.
    crop_drc_counts = Counter()
    if violations:
        for v in violations:
            if only_layer and v.get("layers") and only_layer not in v["layers"]:
                continue
            if not (vx0 <= v["x"] <= vx1 and vy0 <= v["y"] <= vy1):
                continue
            crop_drc_counts[v["type"]] += 1
            vx_px, vy_px = sx(v["x"]), sy(v["y"])
            if v["routed"]:
                lines.append(f'<polygon points="{vx_px:.1f},{vy_px-6.5:.1f} {vx_px+6.5:.1f},{vy_px:.1f} {vx_px:.1f},{vy_px+6.5:.1f} {vx_px-6.5:.1f},{vy_px:.1f}" fill="#f43f5e" stroke="#ffffff" stroke-width="1.1"/>')
                lines.append(f'<text x="{vx_px:.1f}" y="{vy_px+3.2:.1f}" fill="#ffffff" font-size="8.5" font-weight="800" text-anchor="middle">!</text>')
            else:
                # Bare-board pad/mask/edge DRC marker (compact orange X so pad gaps stay clear).
                lines.append(f'<path d="M{vx_px-3.2:.1f},{vy_px-3.2:.1f}L{vx_px+3.2:.1f},{vy_px+3.2:.1f}M{vx_px-3.2:.1f},{vy_px+3.2:.1f}L{vx_px+3.2:.1f},{vy_px-3.2:.1f}" stroke="#fb923c" stroke-width="1.6" stroke-linecap="round"/>')

    # 11. Whole-board overview airwires (when rendering overview).
    if all_airwires:
        show_nums = len(all_airwires) <= 45
        for idx, (a, b) in enumerate(all_airwires, 1):
            x1, y1, x2, y2 = sx(a["x"]), sy(a["y"]), sx(b["x"]), sy(b["y"])
            lines.append(f'<line x1="{x1:.2f}" y1="{y1:.2f}" x2="{x2:.2f}" y2="{y2:.2f}" stroke="#000000" stroke-width="3.6" stroke-opacity="0.7" stroke-linecap="round"/>')
            lines.append(f'<line x1="{x1:.2f}" y1="{y1:.2f}" x2="{x2:.2f}" y2="{y2:.2f}" stroke="#38bdf8" stroke-width="2.0" stroke-dasharray="5,3" stroke-linecap="round"/>')
            lines.append(f'<circle cx="{x1:.2f}" cy="{y1:.2f}" r="3.5" fill="#fef08a" stroke="#0f172a" stroke-width="1"/>')
            lines.append(f'<circle cx="{x2:.2f}" cy="{y2:.2f}" r="3.5" fill="#fef08a" stroke="#0f172a" stroke-width="1"/>')
            if show_nums:
                mx, my = 0.5 * (x1 + x2), 0.5 * (y1 + y2)
                lines.append(f'<rect x="{mx-9:.1f}" y="{my-7:.1f}" width="18" height="14" rx="4" fill="#0f172a" fill-opacity="0.88" stroke="#38bdf8" stroke-width="1"/>')
                lines.append(f'<text x="{mx:.1f}" y="{my+3.5:.1f}" fill="#fef08a" font-size="9" font-weight="700" text-anchor="middle">{idx}</text>')

    # 12. Open connection's two ends and airwire.
    if endpoints:
        a, b = endpoints
        x1, y1, x2, y2 = sx(a["x"]), sy(a["y"]), sx(b["x"]), sy(b["y"])
        lines.append(f'<line x1="{x1:.2f}" y1="{y1:.2f}" x2="{x2:.2f}" y2="{y2:.2f}" stroke="#090c12" stroke-width="4.2" stroke-opacity="0.85" stroke-linecap="round"/>')
        lines.append(f'<line x1="{x1:.2f}" y1="{y1:.2f}" x2="{x2:.2f}" y2="{y2:.2f}" stroke="#38bdf8" stroke-width="2.2" stroke-dasharray="7,4" stroke-linecap="round"/>')
        close_ends = math.hypot(x2 - x1, y2 - y1) < 36
        ret_r = 7.5 if close_ends else 10.0
        for k_ep, (tag, ep, ex, ey) in enumerate((("A", a, x1, y1), ("B", b, x2, y2))):
            lines.append(f'<circle cx="{ex:.2f}" cy="{ey:.2f}" r="{ret_r:.1f}" fill="none" stroke="#38bdf8" stroke-width="1.8" stroke-opacity="0.92"/>')
            lines.append(f'<circle cx="{ex:.2f}" cy="{ey:.2f}" r="2.4" fill="#ffffff" stroke="#0f172a" stroke-width="1.0"/>')
            lbl = f'{tag}: {ep["ref_pin"]}' + (f' [{ep["layer"]}]' if ep["layer"] else "")
            tx = ex + (12 if ex < px + pw - 150 else -12)
            anchor = "start" if ex < px + pw - 150 else "end"
            if close_ends:
                # Offset A and B opposite in Y when very close so both callout pills stay readable.
                a_top = y1 <= y2
                go_up = (k_ep == 0 and a_top) or (k_ep == 1 and not a_top)
                ty = (ey - 13 if ey > py + 28 else ey + 20) if go_up else (ey + 19 if ey < py + ph - 26 else ey - 13)
            else:
                ty = ey - 12 if ey > py + 28 else ey + 20
            tw_est = 6.2 * len(lbl) + 10
            rx0 = tx - 4 if anchor == "start" else tx - tw_est + 4
            lines.append(f'<rect x="{rx0:.1f}" y="{ty - 11:.1f}" width="{tw_est:.1f}" height="15" rx="4" fill="#0b0f17" fill-opacity="0.88" stroke="#38bdf8" stroke-width="1"/>')
            lines.append(f'<text x="{tx:.1f}" y="{ty:.1f}" fill="#f8fafc" font-size="10" font-weight="700" text-anchor="{anchor}">{xml_esc(lbl)}</text>')

    # Scale bar in bottom-left of panel.
    nice_bars = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0]
    target_mm = (pw * 0.16) / scale
    bar_mm = min(nice_bars, key=lambda v: abs(v - target_mm))
    bar_px = bar_mm * scale
    bx0, by0 = px + 12, py + ph - 12
    lines.append(f'<rect x="{bx0 - 4:.1f}" y="{by0 - 16:.1f}" width="{bar_px + 48:.1f}" height="20" rx="4" fill="#0b0f17" fill-opacity="0.78"/>')
    lines.append(f'<line x1="{bx0:.1f}" y1="{by0 - 5:.1f}" x2="{bx0 + bar_px:.1f}" y2="{by0 - 5:.1f}" stroke="#e2e8f0" stroke-width="2.2"/>')
    lines.append(f'<line x1="{bx0:.1f}" y1="{by0 - 9:.1f}" x2="{bx0:.1f}" y2="{by0 - 1:.1f}" stroke="#e2e8f0" stroke-width="1.5"/>')
    lines.append(f'<line x1="{bx0 + bar_px:.1f}" y1="{by0 - 9:.1f}" x2="{bx0 + bar_px:.1f}" y2="{by0 - 1:.1f}" stroke="#e2e8f0" stroke-width="1.5"/>')
    bar_lbl = f"{bar_mm:g} mm"
    lines.append(f'<text x="{bx0 + bar_px + 6:.1f}" y="{by0 - 2:.1f}" fill="#cbd5e1" font-size="9.5" font-weight="600">{bar_lbl}</text>')

    if crop_drc_counts:
        drc_summary = ", ".join(f"{k} x{c}" for k, c in crop_drc_counts.most_common(3))
        lines.append(f'<rect x="{px + pw - 6.2*len(drc_summary) - 22:.1f}" y="{py + ph - 26:.1f}" width="{6.2*len(drc_summary) + 14:.1f}" height="18" rx="4" fill="#0b0f17" fill-opacity="0.82"/>')
        lines.append(f'<text x="{px + pw - 14:.1f}" y="{py + ph - 13:.1f}" fill="#fdba74" font-size="9.5" font-weight="600" text-anchor="end">DRC in view: {xml_esc(drc_summary)}</text>')

    if panel_title:
        tw_p = 6.6 * len(panel_title) + 16
        lines.append(f'<rect x="{px + 8:.1f}" y="{py + 8:.1f}" width="{tw_p:.1f}" height="20" rx="4" fill="#0b0f17" fill-opacity="0.86" stroke="#334155" stroke-width="1"/>')
        lines.append(f'<text x="{px + 16:.1f}" y="{py + 21.5:.1f}" fill="#f1f5f9" font-size="11" font-weight="700">{xml_esc(panel_title)}</text>')

    lines.append("</g>")
    return "\n".join(lines)


def svg_header(width, height):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" '
        f'style="font-family: system-ui, -apple-system, Segoe UI, sans-serif; background: #0c0f16;">\n'
        '<defs>'
        '<pattern id="keepoutHatch" width="8" height="8" patternTransform="rotate(45 0 0)" patternUnits="userSpaceOnUse">'
        '<rect width="8" height="8" fill="#f43f5e" fill-opacity="0.10"/>'
        '<line x1="0" y1="0" x2="0" y2="8" stroke="#fb7185" stroke-width="1.6" stroke-opacity="0.45"/>'
        '</pattern>'
        '</defs>\n'
        f'<rect width="{width}" height="{height}" fill="#0c0f16"/>'
    )


def legend_svg(geom, x, y, width):
    """Consistent legend bar explaining layer colours, pad types, open-net highlight, and markers."""
    items = []
    for cn in geom.cu_names[:8]:
        items.append(("rect", geom.layer_colors[cn], None, cn))
    if len(geom.cu_names) > 8:
        items.append(("rect", "#14b8a6", None, f"+{len(geom.cu_names)-8} inner"))
    items.extend([
        ("rect", "#ca8a04", "#fde047", "PTH pad"),
        ("rect", "#bef264", "#ffffff", "Open net pad/via"),
        ("line_hi", "#ef4444", "#fef08a", "Open net track"),
        ("dash", "#38bdf8", None, "Open airwire + Ends A/B"),
        ("hatch", "#fb7185", None, "Keepout"),
        *( [("mask_open", "#c084fc", None, "Mask opening")] if geom.mask_areas else [] ),
        ("drc", "#f43f5e", "#fb923c", "DRC (routed ! / bare x)"),
    ])
    out = [f'<g transform="translate({x:.1f},{y:.1f})">']
    cx = 0.0
    cy = 12.0
    for kind, c1, c2, label in items:
        est = 22 + 6.3 * len(label) + 14
        if cx + est > width and cx > 0:
            cx = 0.0
            cy += 18.0
        if kind == "rect":
            st = f' stroke="{c2}" stroke-width="1.4"' if c2 else ""
            out.append(f'<rect x="{cx:.1f}" y="{cy - 8:.1f}" width="13" height="10" rx="2" fill="{c1}"{st}/>')
        elif kind == "line_hi":
            out.append(f'<line x1="{cx:.1f}" y1="{cy - 3:.1f}" x2="{cx + 14:.1f}" y2="{cy - 3:.1f}" stroke="{c2}" stroke-width="5" stroke-linecap="round"/>')
            out.append(f'<line x1="{cx:.1f}" y1="{cy - 3:.1f}" x2="{cx + 14:.1f}" y2="{cy - 3:.1f}" stroke="{c1}" stroke-width="2.4" stroke-linecap="round"/>')
        elif kind == "dash":
            out.append(f'<line x1="{cx:.1f}" y1="{cy - 3:.1f}" x2="{cx + 14:.1f}" y2="{cy - 3:.1f}" stroke="{c1}" stroke-width="2.2" stroke-dasharray="4,2"/>')
        elif kind == "hatch":
            out.append(f'<rect x="{cx:.1f}" y="{cy - 8:.1f}" width="13" height="10" rx="2" fill="url(#keepoutHatch)" stroke="{c1}" stroke-width="1"/>')
        elif kind == "mask_open":
            out.append(f'<rect x="{cx:.1f}" y="{cy - 8:.1f}" width="13" height="10" rx="2" fill="{c1}" fill-opacity="0.22" stroke="{c1}" stroke-width="1.2" stroke-dasharray="3,2"/>')
        elif kind == "drc":
            out.append(f'<polygon points="{cx+4:.1f},{cy-8:.1f} {cx+8:.1f},{cy-3:.1f} {cx+4:.1f},{cy+2:.1f} {cx:.1f},{cy-3:.1f}" fill="{c1}"/>')
            out.append(f'<path d="M{cx+10:.1f},{cy-6:.1f}L{cx+15:.1f},{cy-1:.1f}M{cx+10:.1f},{cy-1:.1f}L{cx+15:.1f},{cy-6:.1f}" stroke="{c2}" stroke-width="1.6"/>')
        out.append(f'<text x="{cx + 18:.1f}" y="{cy:.1f}" fill="#cbd5e1" font-size="10.5">{xml_esc(label)}</text>')
        cx += est
    out.append("</g>")
    return "\n".join(out)


def fmt_pitch(info, ep=None):
    if not info:
        if ep and ep.get("kind") != "pad":
            return f"routed {ep['kind']}" + (f" [{ep['layer']}]" if ep.get("layer") else "")
        return "isolated (>8 mm)"
    if info.get("pitch_mm") is None:
        return f"{info['pad']} isolated (>8 mm)"
    gap_s = f", copper gap {info['gap_mm']:.3f} mm" if info.get("gap_mm") is not None else ""
    return f"{info['pad']} pitch {info['pitch_mm']:.3f} mm{gap_s}"


def build_crop_svg(board_id, idx, total_open, geom, a, b, pitch_a, pitch_b, rep_info, rules, violations):
    """Build the composite SVG for one open connection, adding zoom insets when ends are far apart."""
    net = a["net"] or b["net"]
    dist = math.hypot(b["x"] - a["x"], b["y"] - a["y"])
    margin = max(2.6, 0.24 * dist)
    x0 = min(a["x"], b["x"]) - margin
    y0 = min(a["y"], b["y"]) - margin
    x1 = max(a["x"], b["x"]) + margin
    y1 = max(a["y"], b["y"]) + margin
    # Enforce a minimum 4.8 mm span so context around adjacent pads is always visible.
    if x1 - x0 < 4.8:
        mid = 0.5 * (x0 + x1)
        x0, x1 = mid - 2.4, mid + 2.4
    if y1 - y0 < 4.8:
        mid = 0.5 * (y0 + y1)
        y0, y1 = mid - 2.4, mid + 2.4

    far = max(x1 - x0, y1 - y0) > 11.0
    W = 1280 if far else 1020
    H = 870
    title_h = 104
    leg_h = 54
    body_y = title_h + 6
    body_h = H - body_y - leg_h - 8

    parts = [svg_header(W, H)]

    # Title block.
    outline_note = "" if geom.has_outline else " [no Edge.Cuts]"
    layer_desc = f"{len(geom.cu_names)} ({', '.join(geom.cu_names[:6])}{', ...' if len(geom.cu_names) > 6 else ''}){outline_note}"
    ends_desc = f"{a['ref_pin']} <-> {b['ref_pin']}  ({dist:.2f} mm)"
    rule_extras = []
    if rules["pad_mask_margin"] > 0:
        rule_extras.append(f"pad-mask +{rules['pad_mask_margin']:g} mm")
    if rules["mask_to_copper"] > 0:
        rule_extras.append(f"mask-cu {rules['mask_to_copper']:g} mm")
    if rules["dru_rules"]:
        rule_extras.append(f"dru: {len(rules['dru_rules'])} custom")
    extra_s = ("  ·  " + ", ".join(rule_extras)) if rule_extras else ""
    rules_desc = (
        f"Rules [{rules['class']}]: track {rules['track_width']:g} mm (min {rules['min_track_width']:g}), "
        f"clr {rules['clearance']:g} mm, via {rules['via_diameter']:g}/{rules['via_drill']:g} mm, "
        f"edge {rules['edge_clearance']:g} mm{extra_s}"
    )

    parts.append(f'<rect x="12" y="10" width="{W - 24}" height="{title_h - 8}" rx="8" fill="#161b26" stroke="#2c3444" stroke-width="1"/>')
    parts.append(
        f'<text x="24" y="33" fill="#f8fafc" font-size="14.5" font-weight="700">'
        f'{xml_esc(board_id)}  <tspan fill="#94a3b8" font-weight="500">·  open {idx + 1} of {total_open}  ·</tspan>  '
        f'<tspan fill="#bef264">Net {xml_esc(net)}</tspan>  <tspan fill="#94a3b8" font-weight="500">·</tspan>  '
        f'<tspan fill="#38bdf8">{xml_esc(ends_desc)}</tspan></text>'
    )
    parts.append(
        f'<text x="24" y="55" fill="#cbd5e1" font-size="12">'
        f'<tspan fill="#94a3b8">Layers:</tspan> {xml_esc(layer_desc)}   ·   '
        f'<tspan fill="#94a3b8">End A:</tspan> {xml_esc(fmt_pitch(pitch_a, a))}   ·   '
        f'<tspan fill="#94a3b8">End B:</tspan> {xml_esc(fmt_pitch(pitch_b, b))}</text>'
    )
    rep_col = "#f87171" if rep_info["mismatch"] else "#fde047"
    parts.append(
        f'<text x="24" y="76" fill="#cbd5e1" font-size="12">'
        f'<tspan fill="#94a3b8">Router:</tspan> <tspan fill="{rep_col}" font-weight="600">{xml_esc(rep_info["display"])}</tspan>   ·   '
        f'{xml_esc(rules_desc)}</text>'
    )
    if rep_info.get("rule_warnings"):
        rw_short = "; ".join(rep_info["rule_warnings"])[:155]
        parts.append(f'<text x="24" y="94" fill="#fb923c" font-size="11">Rule/board note: {xml_esc(rw_short)}</text>')

    if far:
        main_w = int((W - 36) * 0.64)
        inset_w = W - 36 - main_w
        inset_h = (body_h - 10) / 2.0
        parts.append(render_panel_svg(
            geom, (x0, y0, x1, y1), (12, body_y, main_w, body_h), f"c_main_{idx}",
            open_net=net, endpoints=(a, b), violations=violations, show_pin_labels=True, show_mask_halos=True,
            panel_title="Full connection corridor",
        ))
        zr = 2.4
        parts.append(render_panel_svg(
            geom, (a["x"] - zr, a["y"] - zr, a["x"] + zr, a["y"] + zr),
            (24 + main_w, body_y, inset_w, inset_h), f"c_za_{idx}",
            open_net=net, endpoints=(a, b), violations=violations, show_pin_labels=True, show_mask_halos=True,
            panel_title=f"Zoom End A: {a['ref_pin']}",
        ))
        parts.append(render_panel_svg(
            geom, (b["x"] - zr, b["y"] - zr, b["x"] + zr, b["y"] + zr),
            (24 + main_w, body_y + inset_h + 10, inset_w, inset_h), f"c_zb_{idx}",
            open_net=net, endpoints=(a, b), violations=violations, show_pin_labels=True, show_mask_halos=True,
            panel_title=f"Zoom End B: {b['ref_pin']}",
        ))
    else:
        parts.append(render_panel_svg(
            geom, (x0, y0, x1, y1), (12, body_y, W - 24, body_h), f"c_main_{idx}",
            open_net=net, endpoints=(a, b), violations=violations, show_pin_labels=True, show_mask_halos=True,
        ))

    parts.append(legend_svg(geom, 16, H - leg_h, W - 32))
    parts.append("</svg>")
    return "\n".join(parts), (x0, y0, x1, y1)


def build_split_svg(board_id, idx, geom, crop_box, a, b, violations):
    """Build per-layer split view of the same crop so overlapping copper layers are separated."""
    net = a["net"] or b["net"]
    layers = geom.cu_names
    n = len(layers)
    cols = 2 if n <= 4 else 3 if n <= 6 else 4
    rows = math.ceil(n / cols)
    panel_w = 560 if cols == 2 else 430 if cols == 3 else 340
    panel_h = 480 if rows == 1 else 360 if rows == 2 else 280
    W = cols * panel_w + (cols + 1) * 12
    header_h = 46
    H = header_h + rows * (panel_h + 12) + 8

    parts = [svg_header(W, H)]
    parts.append(
        f'<text x="16" y="29" fill="#f8fafc" font-size="14" font-weight="700">'
        f'{xml_esc(board_id)}  ·  open #{idx + 1} per-layer split  ·  '
        f'<tspan fill="#bef264">Net {xml_esc(net)}</tspan>  ({xml_esc(a["ref_pin"])} &lt;-&gt; {xml_esc(b["ref_pin"])})</text>'
    )
    for k, cn in enumerate(layers):
        r, c = divmod(k, cols)
        px = 12 + c * (panel_w + 12)
        py = header_h + r * (panel_h + 12)
        parts.append(render_panel_svg(
            geom, crop_box, (px, py, panel_w, panel_h), f"split_{idx}_{k}",
            open_net=net, only_layer=cn, endpoints=(a, b), violations=violations,
            show_pin_labels=True, show_mask_halos=False, panel_title=f"Layer {cn}",
        ))
    parts.append("</svg>")
    return "\n".join(parts)


def build_overview_svg(board_id, geom, unconn_parsed, result_meta, violations):
    """Whole-board overview with every open connection's airwire drawn on top."""
    W, H = 1100, 870
    header_h = 62
    leg_h = 54
    body_y = header_h + 6
    body_h = H - body_y - leg_h - 8

    parts = [svg_header(W, H)]
    parts.append(f'<rect x="12" y="10" width="{W - 24}" height="{header_h - 8}" rx="8" fill="#161b26" stroke="#2c3444" stroke-width="1"/>')

    secs = result_meta.get("seconds")
    tl = result_meta.get("time_limited") or []
    tl_str = f"time-limited ({', '.join(tl)})" if tl else "finished early (not time-limited)"
    errs = result_meta.get("new_errors") or {}
    err_str = f"  ·  new errors: {errs}" if errs else ""
    sub = f"{len(unconn_parsed)} open connections  ·  {len(geom.cu_names)} layers  ·  {secs if secs is not None else '?'} s  ·  {tl_str}{err_str}"

    parts.append(f'<text x="24" y="33" fill="#f8fafc" font-size="15" font-weight="700">{xml_esc(board_id)}  <tspan fill="#94a3b8" font-weight="400">: whole-board overview</tspan></text>')
    parts.append(f'<text x="24" y="52" fill="#cbd5e1" font-size="12">{xml_esc(sub)}</text>')

    vbox = grow_box(geom.board_bbox, 1.5)
    parts.append(render_panel_svg(
        geom, vbox, (12, body_y, W - 24, body_h), "ov_main",
        all_airwires=unconn_parsed, violations=violations, show_pin_labels=False,
    ))
    parts.append(legend_svg(geom, 16, H - leg_h, W - 32))
    parts.append("</svg>")
    return "\n".join(parts)


def resolve_board_files(run_board_dir, board_dir=None, report_path=None):
    """Locate PCB, DRC, result, .kicad_pro/.kicad_dru and --report JSON for one board."""
    board_id = run_board_dir.name
    pcb = run_board_dir / "out.filled.kicad_pcb"
    if not pcb.exists():
        pcb = run_board_dir / "out.kicad_pcb"
    drc_path = run_board_dir / "out.drc.json"
    res_path = run_board_dir / "result.json"

    bdir = Path(board_dir) if board_dir else (DEFAULT_BOARDS / board_id)
    pro = None
    for cand in (pcb.with_suffix(".kicad_pro"), run_board_dir / "out.kicad_pro", bdir / "board.kicad_pro"):
        if cand.exists():
            pro = cand
            break
    dru = None
    for cand in (bdir / "board.kicad_dru", run_board_dir / "out.kicad_dru", pcb.with_suffix(".kicad_dru")):
        if cand.exists():
            dru = cand
            break

    rep = Path(report_path) if report_path else None
    if rep is None:
        for cand in (run_board_dir / "out.report.json", run_board_dir / "report.json"):
            if cand.exists():
                rep = cand
                break
    return board_id, pcb, drc_path, res_path, pro, dru, rep


def render_one_board(run_board_dir, out_dir, board_dir=None, report_path=None, causes_for_board=None, skip_existing_images=False):
    """Render overview + every open connection crop + per-layer split for one failed board."""
    run_board_dir = Path(run_board_dir)
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    board_id, pcb, drc_path, res_path, pro, dru, rep_path = resolve_board_files(run_board_dir, board_dir, report_path)
    result_meta = json.loads(res_path.read_text()) if res_path.exists() else {"id": board_id}
    if not pcb.exists() or not drc_path.exists():
        summary = {
            "id": board_id,
            "error": "missing routed board or out.drc.json",
            "unconnected": result_meta.get("unconnected", 0),
            "seconds": result_meta.get("seconds"),
            "time_limited": result_meta.get("time_limited") or [],
            "hard_deadline": bool(result_meta.get("hard_deadline")),
            "new_errors": result_meta.get("new_errors") or {},
            "items": [],
        }
        (out_dir / "board.json").write_text(json.dumps(summary, indent=1))
        return summary

    drc_data = json.loads(drc_path.read_text())
    report_data = json.loads(rep_path.read_text()) if (rep_path and rep_path.exists()) else None
    geom = BoardGeom(pcb, pro, dru)
    violations = relevant_drc_violations(drc_data, geom.alias_to_canon)

    raw_unconn = drc_data.get("unconnected_items") or []
    unconn_parsed = []
    for u in raw_unconn:
        its = u.get("items") or []
        if len(its) >= 2:
            unconn_parsed.append((parse_drc_endpoint(its[0], geom), parse_drc_endpoint(its[1], geom)))

    matched_reports = match_report_failures(unconn_parsed, report_data)

    ov_svg_path = out_dir / "overview.svg"
    ov_png_path = out_dir / "overview.png"
    if not (skip_existing_images and ov_png_path.exists()):
        ov_svg = build_overview_svg(board_id, geom, unconn_parsed, result_meta, violations)
        ov_svg_path.write_text(ov_svg)
        cairosvg.svg2png(bytestring=ov_svg.encode("utf-8"), write_to=str(ov_png_path))

    has_split = len(geom.cu_names) >= 2
    items_meta = []
    board_causes = (causes_for_board or {}).get("items") or {}

    for idx, ((a, b), rep_info) in enumerate(zip(unconn_parsed, matched_reports)):
        net = a["net"] or b["net"]
        rules = geom.rules_for_net(net)
        pitch_a = geom.pitch_near_point(a["x"], a["y"], a["uuid"], net)
        pitch_b = geom.pitch_near_point(b["x"], b["y"], b["uuid"], net)
        dist = round(math.hypot(b["x"] - a["x"], b["y"] - a["y"]), 3)

        crop_svg_name = f"open_{idx:03d}.svg"
        crop_png_name = f"open_{idx:03d}.png"
        split_svg_name = f"open_{idx:03d}_split.svg" if has_split else None
        split_png_name = f"open_{idx:03d}_split.png" if has_split else None

        crop_png_path = out_dir / crop_png_name
        split_png_path = (out_dir / split_png_name) if split_png_name else None

        if not (skip_existing_images and crop_png_path.exists() and (not split_png_path or split_png_path.exists())):
            c_svg, c_box = build_crop_svg(board_id, idx, len(unconn_parsed), geom, a, b, pitch_a, pitch_b, rep_info, rules, violations)
            (out_dir / crop_svg_name).write_text(c_svg)
            cairosvg.svg2png(bytestring=c_svg.encode("utf-8"), write_to=str(crop_png_path))
            if has_split:
                s_svg = build_split_svg(board_id, idx, geom, c_box, a, b, violations)
                (out_dir / split_svg_name).write_text(s_svg)
                cairosvg.svg2png(bytestring=s_svg.encode("utf-8"), write_to=str(split_png_path))

        cause_entry = board_causes.get(str(idx)) or board_causes.get(idx) or {}
        items_meta.append({
            "index": idx,
            "net": net,
            "end_a": a["ref_pin"],
            "end_b": b["ref_pin"],
            "layer_a": a["layer"],
            "layer_b": b["layer"],
            "pos_a": [round(a["x"], 3), round(a["y"], 3)],
            "pos_b": [round(b["x"], 3), round(b["y"], 3)],
            "dist_mm": dist,
            "pitch_a": pitch_a,
            "pitch_b": pitch_b,
            "stage": rep_info["stage"],
            "reason": rep_info["reason"],
            "raw_reason": rep_info.get("raw_reason"),
            "report_display": rep_info["display"],
            "mismatch": rep_info["mismatch"],
            "rules": {
                "class": rules["class"],
                "track_width": rules["track_width"],
                "clearance": rules["clearance"],
                "via_diameter": rules["via_diameter"],
                "via_drill": rules["via_drill"],
                "pad_mask_margin": rules["pad_mask_margin"],
            },
            "crop_png": crop_png_name,
            "crop_svg": crop_svg_name,
            "split_png": split_png_name,
            "split_svg": split_svg_name,
            "cause": cause_entry.get("cause"),
            "note": cause_entry.get("note"),
            "disagrees": bool(cause_entry.get("disagrees", False)),
            "disagree_note": cause_entry.get("disagree_note"),
        })

    # Log fallback stage summary when --report was not run (e.g. the 154 time-limited boards).
    router_open_log = None
    log_path = run_board_dir / "log.txt"
    if report_data:
        router_open_log = report_data.get("summary", {}).get("opens")
    elif log_path.exists():
        m = re.search(r"routed \d+ more, \d+ failed, (\d+) open,", log_path.read_text())
        if m:
            router_open_log = int(m.group(1))

    tl = result_meta.get("time_limited") or []
    hard_dl = bool(result_meta.get("hard_deadline"))
    summary = {
        "id": board_id,
        "layers": len(geom.cu_names),
        "layer_names": geom.cu_names,
        "has_outline": geom.has_outline,
        "keepouts": len(geom.keepouts),
        "unconnected": len(unconn_parsed),
        "drc_unconnected": result_meta.get("unconnected", len(unconn_parsed)),
        "router_open": router_open_log,
        "seconds": result_meta.get("seconds"),
        "vias": result_meta.get("vias"),
        "wirelength": round(result_meta["wirelength"], 1) if result_meta.get("wirelength") is not None else None,
        "time_limited": tl,
        "hard_deadline": hard_dl,
        "is_time_limited": bool(tl or hard_dl),
        "new_errors": result_meta.get("new_errors") or {},
        "has_report": report_data is not None,
        "rule_warnings": [f["message"] for f in (report_data.get("findings", []) if report_data else []) if f.get("kind") in ("rule", "board")],
        "overview_png": "overview.png",
        "overview_svg": "overview.svg",
        "causes": sorted({it["cause"] for it in items_meta if it.get("cause")}),
        "board_note": (causes_for_board or {}).get("board_note"),
        "items": items_meta,
    }
    (out_dir / "board.json").write_text(json.dumps(summary, indent=1))
    return summary


def _worker_star(args):
    return render_one_board(*args)


def render_run(run_dir, out_dir, boards_dir=None, reports_dir=None, causes_path=None, workers=1, skip_existing_images=False, only=None):
    """Render every failed board in a PCBench run directory and write index.json."""
    run_dir = Path(run_dir)
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    causes_map = json.loads(Path(causes_path).read_text()) if (causes_path and Path(causes_path).exists()) else {}

    wanted = set(only.split(",")) if only else None
    failed_entries = []
    for rf in sorted(run_dir.glob("*/result.json")):
        r = json.loads(rf.read_text())
        if r.get("success"):
            continue
        if wanted and r["id"] not in wanted:
            continue
        failed_entries.append((r.get("unconnected") or 0, rf.parent))
    # Schedule boards with the most open connections first so multi-worker runs have no long tail.
    failed_entries.sort(key=lambda x: (-x[0], x[1].name))
    failed_dirs = [d for _, d in failed_entries]

    tasks = []
    for d in failed_dirs:
        bid = d.name
        bdir = (Path(boards_dir) / bid) if boards_dir else (DEFAULT_BOARDS / bid)
        rep = None
        if reports_dir:
            cand = Path(reports_dir) / bid / "out.report.json"
            if cand.exists():
                rep = cand
        tasks.append((d, out_dir / bid, bdir, rep, causes_map.get(bid), skip_existing_images))

    t0 = time.time()
    summaries = []
    if workers > 1 and len(tasks) > 1:
        with concurrent.futures.ProcessPoolExecutor(workers) as pool:
            futs = {pool.submit(_worker_star, t): t[0].name for t in tasks}
            done = 0
            for f in concurrent.futures.as_completed(futs):
                done += 1
                s = f.result()
                summaries.append(s)
                if done % 10 == 0 or done == len(tasks):
                    print(f"[{done}/{len(tasks)}] {s['id']} ({s['unconnected']} open) {time.time() - t0:.1f} s", flush=True)
    else:
        for done, t in enumerate(tasks, 1):
            s = render_one_board(*t)
            summaries.append(s)
            if done % 10 == 0 or done == len(tasks):
                print(f"[{done}/{len(tasks)}] {s['id']} ({s['unconnected']} open) {time.time() - t0:.1f} s", flush=True)

    # If --only was used, keep any existing board.json summaries for the remaining failed boards.
    if wanted:
        by_id = {s["id"]: s for s in summaries}
        for bj in sorted(out_dir.glob("*/board.json")):
            if bj.parent.name not in by_id:
                by_id[bj.parent.name] = json.loads(bj.read_text())
        summaries = list(by_id.values())

    summaries.sort(key=lambda s: s["id"])
    run_meta = json.loads((run_dir / "run.json").read_text()) if (run_dir / "run.json").exists() else {}

    cause_counts = Counter()
    cause_boards = defaultdict(set)
    for s in summaries:
        for it in s.get("items", []):
            if it.get("cause"):
                cause_counts[it["cause"]] += 1
                cause_boards[it["cause"]].add(s["id"])

    index = {
        "run": run_dir.name,
        "generated": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "commit": run_meta.get("commit"),
        "recipe": run_meta.get("recipe"),
        "timeout": run_meta.get("timeout"),
        "seed": run_meta.get("seed"),
        "total_boards_in_run": len(run_meta.get("boards") or []),
        "failed_boards": len(summaries),
        "non_time_limited_boards": sum(1 for s in summaries if not s.get("is_time_limited")),
        "time_limited_boards": sum(1 for s in summaries if s.get("is_time_limited")),
        "total_open_items": sum(s.get("unconnected", 0) for s in summaries),
        "cause_summary": [
            {"cause": c, "connections": n, "boards": len(cause_boards[c])}
            for c, n in cause_counts.most_common()
        ],
        "boards": summaries,
    }
    (out_dir / "index.json").write_text(json.dumps(index, indent=1))
    print(f"Wrote {out_dir / 'index.json'} ({len(summaries)} boards, {index['total_open_items']} open items) in {time.time() - t0:.1f} s")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("target", help="RUN_DIR/ID for one board, or RUN_DIR for every failed board in a run")
    ap.add_argument("--board", help="source board directory with board.kicad_pro / .kicad_dru (single-board mode)")
    ap.add_argument("--boards-dir", help="parent directory of prepared boards (whole-run mode)")
    ap.add_argument("--report", help="router --report JSON file (single-board mode)")
    ap.add_argument("--reports-dir", help="directory of <ID>/out.report.json files (whole-run mode)")
    ap.add_argument("--causes", help="JSON file of cause labels and notes keyed by board id")
    ap.add_argument("--workers", type=int, default=int(os.environ.get("RESERVED_CORES", "1")), help="worker processes in whole-run mode")
    ap.add_argument("--skip-existing", action="store_true", help="reuse existing PNGs and only refresh JSON metadata")
    ap.add_argument("--only", help="comma-separated board IDs in whole-run mode")
    ap.add_argument("-o", "--output", required=True, help="output directory")
    a = ap.parse_args()

    target = Path(a.target)
    if not target.exists():
        sys.exit(f"not found: {target}")

    is_whole_run = (target / "run.json").exists() or (
        not (target / "out.drc.json").exists()
        and not (target / "out.kicad_pcb").exists()
        and any(target.glob("*/result.json"))
    )

    if is_whole_run:
        render_run(
            target,
            a.output,
            boards_dir=a.boards_dir,
            reports_dir=a.reports_dir,
            causes_path=a.causes,
            workers=max(1, a.workers),
            skip_existing_images=a.skip_existing,
            only=a.only,
        )
    else:
        causes_for_board = None
        if a.causes and Path(a.causes).exists():
            causes_for_board = json.loads(Path(a.causes).read_text()).get(target.name)
        s = render_one_board(
            target,
            a.output,
            board_dir=a.board,
            report_path=a.report,
            causes_for_board=causes_for_board,
            skip_existing_images=a.skip_existing,
        )
        print(f"{s['id']}: wrote overview + {s['unconnected']} crops to {a.output}")


if __name__ == "__main__":
    main()
