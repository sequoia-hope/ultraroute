# Ultraroute Studio as a general-purpose routing tool

The studio today is a lab bench: pick a job, pick a router, read arguments and environment
variables, watch frames. A routing tool for anyone with a KiCad board has a different job. It
should take any board, route it well with almost no setup, show plainly what is left and why,
and let a person settle what is left together with the router, then hand the board back to
KiCad. This note is how I would shape it.

## Who and where

A person with a placed KiCad board who wants it routed, or nearly routed, and who will finish
in KiCad. The studio sits beside KiCad, not instead of it: KiCad keeps schematic, placement,
pours and fabrication; the studio owns routing and the arguments about placement that routing
raises. So the loop is: open from KiCad → route → settle what is left → save back to KiCad.

## The flow, as four states of one screen

One primary button in the top bar, whose label follows the board's state:

1. **Open.** Drop a `.kicad_pcb`, pick one, or arrive from KiCad (an action plugin: "Route with
   Ultraroute"). The board shows at once. A short readiness card says what the router sees:
   connections to make, net classes and their widths, rules taken from the project, and
   anything it will not honour (custom `.kicad_dru` rules, slotted holes), so surprises come
   before routing, not after.
2. **Route.** One button. The choices that matter are intent, not recipes: a single
   *speed ↔ thoroughness* control (time budget, negotiation, rip-up, nudge, tighten follow from
   it), *may parts move?* (off, nudge unlocked parts, or nudge anything not locked), and *via
   budget* (few ↔ many). Everything else — router, cell size, environment — goes in an
   Advanced drawer. While it runs, a phase strip (negotiating → finishing → ripping up →
   nudging → tightening) with one progress number, connections made, replaces the frame-rate
   readout. Pause and Stop are always there.
3. **Settle.** What is left is an inbox, not a count. Each open connection is a row: its net,
   its two ends, and *why* it failed in words the router already knows ("walled in by fixed
   copper", "no room to leave pad U4-A2", "congested near U1"), with one or two fixes it can
   try at a click: nudge the blocking part, allow another layer, neck down, rip and reroute the
   area, or route it by hand. Selecting a row frames it on the canvas. Errors, if any, live in
   the same inbox.
4. **Save.** Write the board back: a new file or the original with a backup. The studio runs
   KiCad's own refill and DRC and shows the verdict before the file is handed over, so "it
   routed" means what KiCad says.

## Steering while it runs

The point of a live router is that the person and the router work at once:

- **Drag a part**; the router reroutes only what it touches, live (later: rubber-bands the
  wires as the part moves, as discussed).
- **Lock** a part, a net or a finished route; locked things are never ripped.
- **Brush regions**: *keep out* and *route here* strokes on a layer, felt by the router at once.
- **Sketch a path** for a net with a few clicks; the router follows it as a corridor.
- **Priority**: bump a net to the front, or mark it *do last*.
- **Rip** a selection and let it route again.
- **Undo** as a timeline of board states along the bottom: scrub back to any point, branch from
  there. Every router phase and every human edit is a step on it.

This needs the router as a long-lived process that takes commands while it routes (route,
pause, move part, lock, add region, rip, undo), not a one-shot command restarted on each edit.

## Several answers, not one

The router can produce variants cheaply (different orders, budgets, nudge on/off, layer
preferences). A *Compare* view shows four to six finished candidates as thumbnails with their
numbers — open connections, errors, vias, length, how far parts moved — and the person picks
one, or picks one to keep working on. This is also where the larger goal lands: complete
layouts that differ in size, shape or layer count, each scored, to choose between.

## The screen

```
┌ top bar: board name · state · [ Route ] · Pause · Stop · Save ─────────────────────────┐
│ left: Board            │                canvas                 │ right: Inspector     │
│  layers (focus/hide)   │  focus layer bright, others dim       │  selected part, net, │
│  nets & classes, search│  airlines, congestion, problem marks  │  connection or error │
│  locks                 │  live trial routes while routing      │  with its actions    │
│ ───────────────────────│                                       │ ──────────────────── │
│  Inbox: what is left   │                                       │  Route settings      │
│  (reasons, fixes)      │                                       │  (intent; Advanced)  │
├─ bottom: phase strip while routing · timeline of board states (undo, branch) ─────────┤
```

Keyboard first for the frequent things: `R` route, `Space` pause, `L` lock, arrows nudge, `F`
fit, `1`–`9` focus a layer, `/` find a net or part, `Z` undo.

Canvas conventions: one focused layer drawn bright and the rest dimmed (as KiCad's high-contrast
mode), the hovered net lit across layers, problem markers clickable into the inbox, and the
congestion map as an overlay toggle rather than a panel.

## What moves out

Recordings, jobs, recipes, leaderboards and the bench become a separate **Lab** view for us;
they are how we improve the router, not how anyone uses it.

## Milestones

- **U1 — any board, one button.** Open by path, upload or drop; readiness card; intent
  controls with an Advanced drawer; phase strip; save back with KiCad's refill and DRC shown.
- **U2 — the inbox.** Reasons for every open connection, one-click fixes, problem markers,
  layer focus, find.
- **U3 — steering.** The router as a long-lived process taking commands; drag, lock, brush,
  rip while routing; the undo timeline.
- **U4 — variants.** Run several, compare, pick; later, boards that differ in outline and layers.
- **U5 — KiCad.** The action plugin, then live sync through KiCad's IPC API; the GTK client on
  the same core.

## Open questions for you

- Should the studio ever edit placement beyond nudges (rotate, swap, move groups), or stay a
  routing tool that only asks for room?
- Save back over the original (with a backup) by default, or always to a new file?
- Is the Lab view worth keeping in the same app, or a separate page?
