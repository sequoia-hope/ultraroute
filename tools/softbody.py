"""A soft-body board, after BeamNG's node-and-beam cars: parts are rigid bodies, joined by beams.

Two kinds of beam hold the board together. Connection beams run pad to pad along every ratsnest
edge (a spanning tree per net), so wired parts pull on each other and turn each other. Structure
beams follow the Delaunay mesh of part centres, so squeezing one side spreads through the board
instead of piling parts up against the edge. A part turns as its neighbourhood turns (shape
matching against the mesh), and parts can't overlap or leave the outline, which is a wall: it
pushes, never drags.

The solver is position-based and quasi-static: each outline step warm-starts from the last state
and relaxes the constraints, Jacobi style, so every body takes the average of its corrections.
"""
import math
import re

import numpy as np
from scipy.spatial import Delaunay

GROUND = {"GND", "AGND", "DGND", "PGND"}


def read_pins(dsn_text, unit_mm, sexp_blocks):
    """Each placed part's pins: {ref: {pin: offset from the part's place point, in board mm}}."""
    images = {}
    for block in sexp_blocks(dsn_text, "image "):
        name = re.match(r'\(image\s+("[^"]*"|\S+)', block).group(1).strip('"')
        pins = {}
        for m in re.finditer(r'\(pin\s+("[^"]*"|\S+)\s+(?:\(rotate\s+[-\d.]+\)\s+)?("[^"]*"|\S+)\s+([-\d.]+)\s+([-\d.]+)', block):
            pins[m.group(2).strip('"')] = (float(m.group(3)) * unit_mm, float(m.group(4)) * unit_mm)
        images[name] = pins
    out = {}
    for block in sexp_blocks(dsn_text, "component "):
        image = re.match(r'\(component\s+("[^"]*"|\S+)', block).group(1).strip('"')
        for m in re.finditer(r'\(place\s+("[^"]*"|\S+)\s+([-\d.]+)\s+([-\d.]+)\s+(front|back)\s+([-\d.]+)', block):
            ref = m.group(1).strip('"')
            rot = math.radians(float(m.group(5)))
            c, s = math.cos(rot), math.sin(rot)
            pins = {}
            for pin, (x, y) in images.get(image, {}).items():
                if m.group(4) == "back":
                    x = -x
                pins[pin] = np.array([x * c - y * s, x * s + y * c])
            out[ref] = pins
    return out


def read_nets(dsn_text):
    """Each net's (ref, pin) list."""
    nets = {}
    for name, pins in re.findall(r'\(net\s+("[^"]*"|\S+)\s*\(pins\s+([^)]*)\)', dsn_text):
        nets[name.strip('"')] = [tuple(p.strip('"').rsplit("-", 1)) for p in pins.split()]
    return nets


def spanning_tree(points):
    """Edges of the Euclidean minimum spanning tree (Prim), as index pairs."""
    n = len(points)
    if n < 2:
        return []
    seen = np.zeros(n, bool)
    seen[0] = True
    best = np.linalg.norm(points - points[0], axis=1)
    parent = np.zeros(n, int)
    edges = []
    for _ in range(n - 1):
        cand = np.where(seen, np.inf, best)
        k = int(np.argmin(cand))
        edges.append((int(parent[k]), k))
        seen[k] = True
        d = np.linalg.norm(points - points[k], axis=1)
        closer = d < best
        best = np.where(closer, d, best)
        parent = np.where(closer, k, parent)
    return edges


def rotate(v, a):
    c, s = np.cos(a), np.sin(a)
    return np.stack([v[..., 0] * c - v[..., 1] * s, v[..., 0] * s + v[..., 1] * c], axis=-1)


def cross(a, b):
    return a[..., 0] * b[..., 1] - a[..., 1] * b[...,0]


class SoftBoard:
    def __init__(self, parts, pins, nets, connection_stiffness=0.5, structure_stiffness=0.3,
                 turn_stiffness=0.3, power_pins=12, mesh_mm=15.0):
        self.refs = list(parts)
        self.index = {r: k for k, r in enumerate(self.refs)}
        self.parts = parts
        self.rest = np.array([parts[r]["centre"] for r in self.refs], float)
        size = np.array([parts[r]["box"][1] - parts[r]["box"][0] for r in self.refs])
        self.inertia = (size ** 2).sum(axis=1) / 12.0 + 0.25

        a, b, ra, rb, k = [], [], [], [], []
        for name, members in nets.items():
            if name.upper() in GROUND:
                continue
            pads = [(self.index[ref], pins[ref][pin]) for ref, pin in members
                    if ref in self.index and pin in pins.get(ref, {})]
            if len(pads) < 2:
                continue
            world = np.array([self.rest[i] + off for i, off in pads])
            weight = connection_stiffness * (1.0 if len(pads) <= power_pins else power_pins / len(pads))
            for u, v in spanning_tree(world):
                if pads[u][0] == pads[v][0]:
                    continue
                a.append(pads[u][0]); ra.append(pads[u][1])
                b.append(pads[v][0]); rb.append(pads[v][1])
                k.append(weight)
        tri = Delaunay(self.rest)
        mesh = set()
        for s in tri.simplices:
            for u, v in ((s[0], s[1]), (s[1], s[2]), (s[0], s[2])):
                u, v = min(u, v), max(u, v)
                if np.linalg.norm(self.rest[u] - self.rest[v]) <= mesh_mm:
                    mesh.add((u, v))
        self.mesh = np.array(sorted(mesh))
        for u, v in self.mesh:
            a.append(u); ra.append(np.zeros(2))
            b.append(v); rb.append(np.zeros(2))
            k.append(structure_stiffness)
        self.a, self.b = np.array(a), np.array(b)
        self.ra, self.rb = np.array(ra, float), np.array(rb, float)
        self.k = np.array(k)
        self.length = np.linalg.norm(self.rest[self.b] + self.rb - self.rest[self.a] - self.ra, axis=1)
        self.connections = int((self.ra.any(axis=1) | self.rb.any(axis=1)).sum() + 0)
        self.turn_stiffness = turn_stiffness
        self.neighbours = [[] for _ in self.refs]
        for u, v in self.mesh:
            self.neighbours[u].append(v)
            self.neighbours[v].append(u)

    def beams(self, x, theta):
        pa = x[self.a] + rotate(self.ra, theta[self.a])
        pb = x[self.b] + rotate(self.rb, theta[self.b])
        return pa, pb

    def strain(self, x, theta):
        """Each beam's length over its rest length, connection beams then structure beams."""
        pa, pb = self.beams(x, theta)
        ratio = np.linalg.norm(pb - pa, axis=1) / np.maximum(self.length, 1e-6)
        conn = len(self.k) - len(self.mesh)
        return ratio[:conn], ratio[conn:]

    def solve_beams(self, x, theta, pinned):
        pa, pb = self.beams(x, theta)
        d = pb - pa
        dist = np.linalg.norm(d, axis=1)
        n = d / np.maximum(dist, 1e-9)[:, None]
        c = dist - self.length
        arm_a, arm_b = rotate(self.ra, theta[self.a]), rotate(self.rb, theta[self.b])
        free = (~pinned).astype(float)
        wa = free[self.a] * (1.0 + cross(arm_a, n) ** 2 / self.inertia[self.a])
        wb = free[self.b] * (1.0 + cross(arm_b, n) ** 2 / self.inertia[self.b])
        lam = self.k * c / np.maximum(wa + wb, 1e-9)
        dx, dt, count = np.zeros_like(x), np.zeros(len(x)), np.zeros(len(x))
        np.add.at(dx, self.a, free[self.a, None] * lam[:, None] * n)
        np.add.at(dx, self.b, -free[self.b, None] * lam[:, None] * n)
        np.add.at(dt, self.a, free[self.a] * lam * cross(arm_a, n) / self.inertia[self.a])
        np.add.at(dt, self.b, -free[self.b] * lam * cross(arm_b, n) / self.inertia[self.b])
        np.add.at(count, self.a, 1)
        np.add.at(count, self.b, 1)
        count = np.maximum(count, 1)
        return x + dx / count[:, None], theta + dt / count

    def solve_turns(self, x, theta, pinned):
        """Each part turns toward the rotation that best carries its mesh neighbours from rest."""
        out = theta.copy()
        for i, nbrs in enumerate(self.neighbours):
            if pinned[i] or not nbrs:
                continue
            v0 = self.rest[nbrs] - self.rest[i]
            v = x[nbrs] - x[i]
            phi = math.atan2(cross(v0, v).sum(), (v0 * v).sum())
            delta = (phi - theta[i] + math.pi) % (2 * math.pi) - math.pi
            out[i] = theta[i] + self.turn_stiffness * delta
        return out

    def solve(self, x, theta, pinned, poly, contacts, walls, iterations=80, settle=1500, turns=None):
        """Relaxes from (x, theta); `pinned` bodies keep where they are. `contacts(x, theta)`
        returns overlap pairs as {(i, j): (axis from i to j, depth)} and `walls(x, theta)` the
        push that brings each stray body back inside `poly`, as {i: vector}. With `turns`, parts
        hold those angles and only their positions are soft."""
        for _ in range(iterations):
            x, theta = self.solve_beams(x, theta, pinned)
            theta = self.solve_turns(x, theta, pinned) if turns is None else turns
            for _ in range(3):
                x = self.push_apart(x, theta, pinned, contacts, walls)
        for _ in range(settle):
            moved = self.push_apart(x, theta, pinned, contacts, walls)
            if np.array_equal(moved, x):
                break
            x = moved
        return x, theta

    @staticmethod
    def push_apart(x, theta, pinned, contacts, walls):
        """One round of contacts and walls, which win over the beams: overlaps and parts past
        the edge are what the plan must never show."""
        shift = np.zeros_like(x)
        count = np.zeros(len(x))
        for (i, j), (axis, depth) in contacts(x, theta).items():
            wi, wj = (0.0 if pinned[i] else 1.0), (0.0 if pinned[j] else 1.0)
            if wi + wj == 0:
                continue
            push = axis * (depth + 0.05)
            shift[i] -= push * wi / (wi + wj)
            shift[j] += push * wj / (wi + wj)
            count[i] += wi > 0
            count[j] += wj > 0
        x = x + shift / np.maximum(count, 1)[:, None] * np.minimum(count, 2)[:, None] * 0.5
        for i, v in walls(x, theta).items():
            if not pinned[i]:
                x[i] = x[i] + v
        return x
