# Using the core as a library

The router is a set of Rust crates under `core/crates`. The `ultraroute` command is one program
built on them; the studio's WebAssembly build is another. This guide is for a third: a program of
your own that reads a board, routes or reworks it with the stages it wants, and writes it back.

A working example walks the whole path in about 140 lines:
`core/crates/ultraroute/examples/library_tour.rs`. Run it with

    cargo run --release -p ultraroute --example library_tour -- core/fixtures/earring.kicad_pcb /tmp/out.kicad_pcb 20

Read it beside this guide.

## The crates and how data flows

| crate | what it is |
|---|---|
| `kicad-pcb` | Reads `.kicad_pcb` text into a neutral record (`Pcb`, `Copper`), the rules from `.kicad_pro` and `.kicad_dru` (`Rules`), and writes edits back into the text (`apply`, `Edit`). Metres. |
| `board` | The routing model: every piece of copper, hole, keepout and the outline as `Item`s in a spatial index (`Board`), the rule check (`check_item`), connectivity (`connectivity`), zone fills for display. Nanometres. |
| `grid` | The grid routers and everything that reworks routed copper: `route_open`, `route_negotiated`, `rip_up`, `chase`, `prefanout`, `route_pairs`, `join_planes`, `shove_escape` (the plough), `push`, `nudge`, `pull_tight`, cleanup, `match_lengths`, `blame`. Also the grid itself (`ObstacleMap`, `RouteGrid`, `Profile`) and the `FieldSolver` trait with CPU and GPU solvers. |
| `morph` | Moving copper smoothly: the harmonic `WarpField`, `flow` (copper carried along with moving parts), `shift_copper` (any per-point move that keeps joints joined), and `fine_pitch` (the parts whose escapes are hard). |
| `topo` | Topological routing: rubber bands pulled taut through a triangulation of the copper. |
| `ultraroute` | The command: `parse` turns flags into `Args`, `run` drives the whole pipeline from text to text, `Live` streams the studio's frames. |
| `web` | The same pipeline compiled to WebAssembly for the studio's Web Worker. |

A run goes like this:

1. Read the board's text. `kicad_pcb::parse_board(&text)` gives a `Pcb` and its `Copper`.
   `Rules::read(path_to_kicad_pro)` gives the rules, with the `.kicad_dru` beside it applied.
2. `Board::new(&pcb, &copper, rules)` builds the model. `Board::load(path)` does steps 1 and 2 and
   returns the text too, which you must keep: writing back edits it in place.
3. The routers mutate the `Board`. Each lays tracks and vias with `board.insert` after checking
   them, and reports the ids it laid. Your program keeps one list of the router's own copper,
   called `soft` in the grid's signatures and `laid` in `run`. That list is what later stages may
   rip, move and tighten, and what is written back.
4. Whatever leaves the board that came from the file (stripped, ripped, moved) has a `Span`, the
   byte range of its text. Keep every such span in a `removed` list.
5. `grid::keep_orphans(&board, &removed, &mut laid)` adds file copper that shares a removed span
   (an arc's other chords), then `grid::edits(&board, &laid)` turns the router's copper into `NewTrack`s and `NewVia`s, and
   `kicad_pcb::apply(&text, &Edit { remove, replace, tracks, vias })` returns the new file text:
   spans deleted, new copper appended before the final paren, every other byte as it was.

```rust
use board::{connectivity_without_pours, Board};
use kicad_pcb::{apply, Edit};

let (mut board, text) = Board::load("in.kicad_pcb".as_ref())?;
let mut solver = grid::CpuSolver;
let mut laid = Vec::new();
let stats = grid::route_open(&mut board, &mut solver, &grid::Options::default(), &mut |_| {});
laid.extend(stats.laid.iter().copied());
println!("{} open", connectivity_without_pours(&board).airlines.len());
let (tracks, vias) = grid::edits(&board, &laid);
std::fs::write("out.kicad_pcb", apply(&text, &Edit { tracks, vias, ..Edit::default() }))?;
```

Nothing here is global or threaded behind your back, except negotiation when you ask it for
threads. A `Board` is `Clone`, and cloning it is the usual way to try something and undo it.

## Conventions

**Units.** `board` works in integer nanometres (`board::Nm` is `i64`; `board::nm(metres)` and
`Point::from_m([x, y])` convert). `kicad-pcb` works in metres as `f64`, so `Rules`, `Pcb` and
`NewTrack` are metres. The command line takes millimetres. Every `cell`, `reach`, `radius`,
`ring` or `zone` field in an options struct is nanometres: `cell: 50_000` is 0.05 mm. Stats
that report lengths say so (`CleanStats::length_before` and `TuneStats::skews` are mm,
`PairStats::coupled_mm` too).

**Search costs** are not lengths. The grid search charges 10 per straight step and 14 per
diagonal (`grid::search::STRAIGHT`, `DIAGONAL`), so `via_cost: 80` makes a via cost as much as
eight cells of track, `rip_cost: 400` forty. `Board::via_avoid_cost` (3,000 by default) is in
the same units. `topo::Options::via_cost` and `rip_cost` are different: lengths in nanometres.

**Coordinates.** The board model keeps KiCad's file frame: x right, y down, in nanometres.
`Pcb` is the same frame in metres. Two places flip y:

- The live stream (`Live`, the studio) draws y up, so its millimetres are `(x, -y)`.
- The command's `--place REF=X,Y` takes y up too: `run` builds the point as
  `Point::new(x * 1e6, -y * 1e6)`. To place a part at KiCad's (100, 97.5) you pass
  `--place U1=100,-97.5`.
- `--rip-region X,Y,R` and `--via-avoid X0,Y0,X1,Y1` do not flip: they are KiCad's frame.

In library code there is no flip anywhere: build `Point`s in the board's frame.

**Layers** are bitmasks. `Board::layers` lists the copper layers by name in the file's order, and
bit `i` of `Item::layers` (a `u64`) means `board.layers[i]`. `item.on(i)` tests one. A through
via has every bit from its first to its last layer. `Board::near(layer, bbox)` finds items on
one layer; `layer == board.layers.len()` asks for holes instead.

**Nets.** `Item::net` indexes `Board::nets`, the names. Net 0 is no net: its name is empty, and a
router never routes it. `board.net_name(n)` is the safe lookup; `board.clearance(a, b)` gives the
clearance two nets' copper must keep, in nanometres, from their classes.

**Kinds.** `Kind::Pad { footprint, number, plated }`, `Track`, `Via`,
`Pour { zone, kept }` and `Keepout { forbids, footprint }`. Tracks and vias are
`Shape::Capsule { a, b, r }` (a via has `a == b`); pads, pours and keepouts are capsules or
`Shape::Polygon`. A KiCad arc becomes several straight track items that share the arc's one
span; call `grid::keep_orphans(&board, &removed, &mut laid)` before `edits` so that, when a stage
ripped or moved one chord (taking the arc's text with it), the others are written anew.

**Pours.** A pour that KiCad refills (`kept: false`) is normally not an obstacle: the routers
route through it and leave KiCad to pour round the new copper. Every stage's `pours_block` option
makes pours walls instead. Count open connections to match: `connectivity_without_pours(&board)`
when pours are refilled (the default), `connectivity(&board)` when they block. Pours of a
`Block` (a KiCad group) are `kept: true`: planes other nets keep clear of either way.
`Board::set_planes(mask)` turns whole layers into planes: no tracks, vias pass, and their pours
join their nets.

**Locked.** `Item::locked` marks copper no stage may move or rip. Pads, pours and keepouts are
always locked; tracks and vias carry the file's `locked` flag. `FootprintInfo::locked` holds a
part still in `push` and `nudge`; parts in a KiCad group are locked too. Set
`board.footprints[i].locked = true` to hold more (the command's `--lock`).

**Spans.** `Item::span` is the item's text in the board file. Copper the router laid has none.
When a stage moves file copper it takes the span off the item, reports it in its stats
(`removed_spans`, `ripped_spans`), and puts the moved item in `soft` to be written as new copper.
Collect those spans; if you miss one, the old copper stays in the file beside the new.
To take over the file's own unlocked copper as the router's (the command's `--adopt`):

```rust
for id in ids_of_unlocked_tracks_and_vias {
    if let Some(mut it) = board.remove(id) {
        removed.extend(it.span.take());
        board.restore(id, it); // same id, no span: it will be written from `laid`
        laid.push(id);
    }
}
```

**Soft copper.** The grid's stages differ in what they may touch. `rip_up`, `chase`,
`pull_tight`, `drop_duplicates`, `prune_dangling` and `match_lengths` work only on the ids in
`soft`. `push`, `nudge`, `shove_escape`, `rip_broken`, `morph::flow` and `morph::shift_copper`
move or rip any unlocked track or via, file copper included, and add what they move to `soft`.
`route_open`, `route_negotiated`, `prefanout`, `route_pairs`, `join_planes` and `topo` do not
take `soft`: they return what they laid in their stats' `laid`, and you add it yourself. Forget
that and `rip_up` treats the copper as fixed and `edits` never writes it.

**Time, cancelling and progress.** Most options structs have `max_seconds: Option<f64>` (that
stage only; there is no global clock), `cancel: Option<Arc<AtomicBool>>` (set it from anywhere
to stop at the next connection) and `log: Option<grid::Log>`. A `Log` is
`Log(Arc<dyn Fn(Activity) + Send + Sync>)`; each `Activity` has a `phase`, a `text` and an
optional `Focus` (a connection, a part, or a part's trial move). Every stage also takes a
`watch: &mut dyn FnMut(&Board)` called whenever the board changes in a way worth drawing; pass
`&mut |_| {}` when you do not draw. The `*_watch` variants report richer events.

```rust
let log = grid::Log(std::sync::Arc::new(|a: grid::Activity| eprintln!("{}: {}", a.phase, a.text)));
let options = grid::Options { log: Some(log.clone()), ..Default::default() };
```

## kicad-pcb: reading and writing files

- `parse_board(&text) -> Result<(Pcb, Copper), KicadParse>`: the board and its copper.
  `parse_kicad_pcb` gives the `Pcb` alone. KiCad 6 and later.
- `Pcb`: `nets`, `footprints` (reference, value, `at`, rotation, pads), the outline primitives,
  thickness, `warnings`. `pcb.outline_loops()` chains the Edge.Cuts into loops.
- `Copper`: `layers`, `tracks`, `arcs`, `vias`, `zones`, `pads`, `groups`, courtyards, mask
  settings, each with its `Span`.
- `Rules::read(&Path)` reads a `.kicad_pro` and applies the `.kicad_dru` beside it;
  `Rules::parse(&str)` reads the project text alone, and `rules.apply_custom(dru_text, name)`
  adds custom rules. It understands per-net or per-class `track_width` minimums, through-hole
  `hole_clearance` and `hole_to_hole`, and SMD to SMD `clearance`. Any other custom rule lands
  in `rules.warnings` and is not applied: print them. `class_of(net)`, `clearance(a, b)` and
  `min_track_width_of(net)` answer by net name, in metres.
- `apply(&text, &Edit) -> String`. `Edit { remove: Vec<Span>, replace: Vec<(Span, String)>,
  tracks: Vec<NewTrack>, vias: Vec<NewVia> }`. A removed item alone on its line takes the line
  with it; overlapping spans are taken once. New items get fresh uuids.
- `mm(metres) -> String` formats a length as KiCad writes it.

## board: the model

- `Board::load(&Path) -> Result<(Board, String), LoadError>` and `Board::new(&pcb, &copper, rules)`.
- Reading: `items()` (id and item, in id order), `item(id)`, `bbox(id)`,
  `near(layer, bbox)`, `footprints`, `nets`, `layers`, `outline`, `rules`.
- Changing: `insert(item) -> ItemId`, `remove(id) -> Option<Item>`, `restore(id, item)` (put an
  item back under its old id), `discard(id)` (remove a trial insert and reuse its id).
  Ids are indices into a slot list: a removed id stays empty until restored.
- Parts: `move_footprint(f, by)` moves pads and rule areas and takes out, and returns, the
  unlocked tracks and vias that touched them. `shift_footprint(f, by)` moves the part and leaves
  all copper where it is. Both update `footprints[f].at` and the courtyard.
- Checks: `check_item(&board, id)` lists what one item breaks against everything else, pours
  left out because they are refilled; `check_item_with_pours` counts them;
  `check(&board)` checks the whole board. A `Violation` names the `rule`
  (`Clearance`, `EdgeClearance`, `HoleClearance`, `HoleToHole`, `Keepout`), the items `a` and
  `b`, the `layer`, the `gap` found and the gap `required`, in nanometres. The routers check
  every piece before they keep it, so a clean run's copper checks clean; check again after your
  own edits. Solder mask openings (`Board::mask_openings`, `via_openings`, `mask_areas`) are
  part of the check.
- Connectivity: `connectivity(&board)` and `connectivity_without_pours(&board)` give each copper
  item's `cluster` and the `airlines` still open. An `Airline` has the `net`, the points
  `from` and `to`, and the items `from_item` and `to_item` at its ends. `joined(&board, a, b)`
  says whether two items connect as KiCad would join them.
- `fill_zones(&board, &copper)` pours the zones on a 25 µm raster, for drawing only.
- Areas: `Board::via_avoid` holds shapes where a via costs `via_avoid_cost` extra (rule areas
  named `soft-novia...` start there); push a `Shape::Polygon` to add one.

## grid: routing

All grid routers take a `&mut dyn FieldSolver`. `grid::CpuSolver` is a unit struct. With the
`gpu` feature (on in `ultraroute` by default, off in `web`), `grid::GpuSolver::new()` returns
`Option<GpuSolver>`, `None` without a usable adapter. The solver only finds cheapest paths, so
every router gives the same kind of result on either.

### route_open

`route_open(board, solver, &Options, watch) -> Stats` routes open connections one at a time,
shortest first, and goes round again while a round joins anything.

| `grid::Options` | default | |
|---|---|---|
| `cell` | 50,000 nm | grid step |
| `via_cost` | 80 | search units |
| `max_seconds` | None | |
| `nets` | None | only these nets, when set |
| `pours_block` | false | |
| `seed` | 0 | reorders connections of nearly equal length, for searches over runs |
| `cancel`, `log` | None | |

`Stats`: `routed`, `failed` (split into `no_terminal`, `no_path`, `refused`), `vias`, `laid`,
and `failures` (each end item and the reason).

### route_negotiated

`route_negotiated(board, solver, &NegotiationOptions, watch, trials) -> NegotiationStats` is
PathFinder: every connection routes against the rest, cells another net claims cost a present
price that grows each round plus a history that builds where nets keep colliding, until no two
collide; then the paths are committed, each checked. It routes the bulk of a board far better
than `route_open` alone, which is then left to finish. `trials` sees each round's trial paths;
`route_negotiated_rounds` adds a callback with each round's congestion map.

| `NegotiationOptions` | default | |
|---|---|---|
| `cell` | 50,000 nm | |
| `via_cost` | 200 | higher than the other stages' 80 |
| `max_rounds` | 30 | |
| `present` | 20.0 | price per other net on a cell, first round |
| `present_growth` | 1.6 | its growth per round |
| `history_step` | 10 | added where a path collides |
| `patience` | 4 | rounds without fewer collisions before giving up |
| `threads` | 1 | above 1, each round routes every connection at once on that many threads |
| `nets` | None | |
| `net_weight` | empty | per-net multiplier on the prices; below 1 also routes that net first |
| `via_zones` | true | zones as tight as the grid allows, and vias negotiated: priced where they would crowd another net's trace or via, and kept hole-to-hole apart within a net unless shared |
| `repasses` | 2 | passes after the first over what it left open, routed round what it laid |
| `rip_blocked` | true | a connection walled off by routes it laid has them ripped and negotiated over with it, kept only when fewer are left open |
| `reachable_tree` | true | a net's tree spanned by pairs that can meet first, so a pocketed middle pin does not keep the pins either side apart |
| `seed`, `pours_block`, `max_seconds`, `cancel`, `log` | | as above |

`NegotiationStats`: `connections`, `rounds`, `conflicted`, `committed`, `unroutable`, `vias`,
`laid`.

### rip_up and chase

`rip_up(board, solver, &RipOptions, soft, watch) -> RipStats` tries each open connection
through the router's own copper at a price (`rip_cost`, default 400), rips what the new path
meets, routes what that opened, and keeps the board only if fewer connections are open.
Rounds repeat while any closes. `RipOptions`: `cell` 50,000, `via_cost` 80, `rip_cost` 400,
`reroute_seconds` 10 (per attempt), `max_seconds`, `pours_block`, `cancel`, `log`.

`chase(board, solver, &ChaseOptions, soft, watch) -> ChaseStats` is rip-up that chains: what
one link rips is laid the same way and may rip in turn, never undoing an earlier link.
`ChaseOptions`: `rip_cost` 400, `chain_cost` 3,000 (crossing the chain's own new copper),
`links` 24, plus the usual.

### prefanout, planes and pairs

These run before routing in the command, so their copper is in place first.

- `prefanout(board, &PrefanoutOptions, watch) -> PrefanoutStats` lays an escape from each pad
  of the given fine-pitch parts out to free cells of the global grid. `PrefanoutOptions`:
  `parts` (footprint indices; empty does nothing; the command uses `morph::fine_pitch(&board,
  500_000)`), `local_cell` 10,000, `global_cell` 50,000, `ring` 1,100,000 (how far past the
  pad box the escapes end), `mode` (`EscapeMode::Stub`, `Dogbone`, `GridRing`, default
  `GridInner`), `order` (`EscapeOrder::Round`, default `InnerFirst`, `TrappedFirst`), `inward`
  true (first bridge pads to their own kept pour, exposed pad or neighbouring pad),
  `skip_nets`, `pours_block`, `cancel`, `log`. The command keeps the escapes in a list apart
  from `laid` until routing is done, then `prune_closed_escapes(board, &mut escapes,
  pours_block)` takes out the unused ones of nets already joined.
- `join_planes(board, &log) -> PlaneStats` gives every pad of a plane's net that is not on
  its plane a via down to it. Call it after `set_planes`; its `laid` is soft.
- `diff_pairs(&board) -> Vec<Pair>` finds pairs by net name (names alike but for one `P`
  or `+` against `N`, `M` or `-`, standing alone or after a `D`: `USB_DP`/`USB_DM`,
  `CLK_P`/`CLK_N`, `D+`/`D-`), each with its `p`, `n`, `width` and `gap` from the class. `route_pairs(board,
  &PairOptions { cell, pours_block, log }, watch) -> PairStats` lays their coupled runs; keep
  that `laid` out of `soft` until the end so nothing rips a pair apart. `shares_layer(&board,
  &pair)` says whether a pair's ends share a layer. After routing, `match_lengths(board,
  &pairs, soft) -> TuneStats` adds bumps to the shorter net; `pair_lengths` measures.

### tidying

- `pull_tight(board, soft) -> CleanStats` replaces each chain of the router's tracks between
  fixed points with the fewest straight tracks that keep every rule. Ends stay put.
- `drop_duplicates(board, soft)` and `prune_dangling(board, soft)` return `(count, spans)`:
  doubled tracks, and copper leading nowhere, taken out. Add the spans to `removed`.

### the grid itself

For tools of your own: `Profile::for_net(&board, net)` gives a net's sizes from its class
(`half_width`, `clearance`, `via_radius`, `drill_radius`, the neck widths near pads).
`ObstacleMap::new(&board, profile, cell, pours_block)` stamps every item's claim on a grid one
class shares (`window` covers a box, `without` leaves items out, `stamp` adds one).
`map.view(net)` is a `RouteGrid`: `blocked` and `via_blocked` per cell, `penalty` costs, and
`index`, `centre`, `cell_at` to move between cells and points. `terminal_cells(&mut board,
&grid, item)` lists the free cells a path to an item may end on, and `solver.route(&grid,
&sources, &targets, via_cost)` returns a `grid::search::GridPath`, a list of
`(layer index into grid.layers, i, j)`. `FieldSolver` needs one method, `solve(grid,
sources, via_cost) -> Vec<u32>` (cost to every node); the rest have defaults. A custom solver
reaches the whole pipeline through `ultraroute::run_with_solver`; the studio's worker pool is
one.

`blame(&mut board, from_item, to_item, &BlameOptions, &movable) -> BlameReport` says what is in
an open connection's way: nets whose copper the cheapest path crosses, parts that cannot move,
and for parts that can (`movable(footprint)`), the move (`Blame { footprint, dx, dy }`, nm)
that clears it. `BlameOptions::routed_copper: None` makes routed copper a wall, so only part
moves are offered.

## topo: rubber bands

`topo::route_topological(board, &topo::Options, watch) -> topo::Stats` routes open connections
as taut wires through a triangulation of the copper, with its own rip-up.
`route_topological_with_soft(board, &options, soft, watch)` may also rip earlier grid copper in
`soft`. `topo::Options`: `sample` 100,000 nm (outline sample spacing; keep it below twice the
narrowest clearance reach), `via_cost` 2,000,000 nm, `max_expansions` 400,000, `orders` 1,
`rip_cost` 1,000,000 nm, `rip_passes` 6 (0 turns rip-up off), `only` (a set of
`(min_id, max_id)` connections), `nets`, `pours_block`, `cancel`, `log`, `max_seconds`.

## Pushes, fields and morphs

Everything here moves copper, or parts and copper, by a smooth displacement field instead of
ripping and rerouting. The field decides where each point goes; the machinery underneath keeps
joints joined, holds pads and locked copper still, and leaves rule breaking to be ripped. A
field is a displacement in nanometres, not a force: one call is one move.

Two layers do the moving:

- **`morph::shift_copper(board, soft, piece, rule) -> (first, second, spans)`** moves every
  unlocked track and via by `rule(point, net, layer) -> Option<(Point, bool)>`, a point at a
  time. Tracks are cut into pieces no longer than `piece` nanometres so they bend. A point
  touching a pad, or locked copper of its net, stays put; copper ending in a via moves as the
  via's centre does (the rule sees `layer: None` there), so everything meeting at a point gets
  the same move. The `bool` sorts moves into two counts. Moved file copper's spans come back.
  Every copper morph below is a `rule` for this function.
- **`morph::flow(board, &moves, soft) -> FlowStats`** moves parts and carries the copper with
  them. `moves` maps footprint index to its move. It builds `warp_for(board, &moves)`: a
  `WarpField` rigid on each moving part (its pads and their hull move by its vector), zero on
  every still part, keepout, locked track and kept pour, and harmonic in between. Each free
  track is cut into 0.5 mm pieces and every point moves by the field, so copper flows round
  moving parts and nothing tears at a pad. It moves the parts too.

### WarpField

`morph::WarpField` is the harmonic field on its own, for building moves of your own.

```rust
let mut field = morph::WarpField::new(bounds, 150_000.0, 3_000_000.0); // domain, cell, margin (nm)
field.add_inclusion(&pad_shapes, (200_000.0, 0.0), true); // these shapes move 0.2 mm right; true takes their hull too
field.add_inclusion(&still_shapes, (0.0, 0.0), false);    // these hold still
field.solve();                                            // required before displacement
let (dx, dy) = field.displacement(point);                 // bilinear, nm
```

Inside an inclusion the field is its vector; on the domain's border (the bounds plus the margin)
it is zero; elsewhere it solves Laplace's equation, so it is as smooth as a field fixed at those
places can be. Shapes claim cells before hulls do, and earlier inclusions before later.
`may_move(bbox)` says whether anything in a box would move by a nanometre or more.

### push: a field that moves parts

`grid::push(board, &field, &PushOptions, soft, watch) -> PushStats` moves every unlocked part by
a field and lets the copper flow with it. The field is any `Fn(Point) -> (f64, f64)`: a point's
move in nanometres.

Each step (`steps`, default 20) moves each part by the field at its pads' centre divided by the
steps, trying a full, half and quarter step and keeping the first that leaves its pads clear of
everything that will not flow away (pads, locked copper, keepouts, the edge, other parts' holes
and courtyards on its side). Then `morph::flow` carries the copper, and `rip_broken` takes out
what the flow pressed into something. Route again after: the command runs `route_open`.

`PushOptions`: `steps` 20, `locked` (footprints held besides those locked on the board), `only`
(when set, only these move), `pours_block`, `log`. `PushStats`: `moved` (each part and its total
move), `held_back`, `flowed`, `ripped`, `removed_spans`. `push_watch` reports each `PushStep`.

The field moves parts, not copper. Copper only moves because parts did: a push with no unlocked
part in reach changes nothing. To move copper by a field directly, use `shift_copper`; copper it
pushes into something breaks a rule, so rip it with `grid::rip_broken(board, soft, pours_block)`
and route again. The example does exactly this, and undoes the move when it leaves more open.
Moved parts must be written back (see the last section). The command pushes only as `--nudge`
does, one part at a time.

### nudge: small part moves, judged

`grid::nudge(board, solver, &NudgeOptions, soft, watch) -> NudgeStats` tries moving parts near
each open connection. Each try is a one-step `push` of one part (the step at the part's centre,
fading as `1 / (1 + d²/r²)`, `r` the courtyard's half-diagonal plus 0.5 mm), legal only if the part's
pads keep every rule and its courtyard meets no other; what it ripped is routed again, and the
move is kept only if fewer connections are open. `blame` suggests moves first.

`NudgeOptions`: `steps` `[250_000, 500_000, 1_000_000]` nm, `reach` 2,000,000 nm (how near an
airline a courtyard must come), `parts_per_connection` 6, `reroute_seconds` 5, `locked`,
`blind` true (also try the eight directions, not only blame's moves; slow), `cell`, `via_cost`
80, `max_seconds`, `pours_block`, `cancel`, `log`. `NudgeStats`: `tried`, `illegal`, `kept`,
`guided`, `moved`, `ripped_spans`. `nudge_watch` reports `NudgeEvent::Blame` and `Step`.

### shove_escape: the plough

`grid::shove_escape(board, solver, &ShoveOptions, soft, watch) -> ShoveStats` frees connections
trapped at fine-pitch pads. For each open connection with an end on one of `parts` (every open
connection when `None`), it finds the cheapest path with other nets' movable copper passable at a
price (`soft_cost` 30 near it, `cross_cost` 400 on it), then clears that copper off the path.
The trapped connection is routed first, then everything opened, and the try is kept only if
fewer connections are open (or as many, with `accept_ties`). A round that leaves it trapped is
followed by another along the new path, up to `rounds`, laying the path outright.

`Plough` picks how the copper in the way is cleared.

| `Plough` | clearing |
|---|---|
| `Region` (default) | Every other net's movable copper within the reach of where the path is pinched is ripped, and the trapped connection and all it opened negotiate together, the trapped net's congestion at half price. |
| `Rip` | The path is laid outright, ripping exactly what its pieces would break a rule against. |

Where a round cannot lay the path, copper crossing it, and vias on it, are ripped, and the rest
moves straight away from it by `R (1 - d/W)`, `W = max(reach, 2R)`, with `morph::shift_copper`:
`R` is the clearance a net's copper needs from the path (the path's half width or via radius, the
two nets' clearance, the net's widest piece and `margin`).

`ShoveOptions`: `cell` 50,000, `via_cost` 80, `rounds` 4, `plough` `Region`, `reach` 2,000,000
nm, `reaches` and `net_reaches` (per-pad and per-net reach overrides), `margin` 50,000 nm,
`soft_cost` 30, `cross_cost` 400, `cascade` 0 (connections a try knocked open that are ploughed
in turn), `parts` None, `reroute_seconds` 20, `repair_seconds` 10, `seed` 0, `accept_ties`
false, `deadline`, `pours_block`, `log`. `ShoveStats` has a `ShoveTry` per connection (rounds,
`routed`, `kept`, open before and after) and `removed_spans`.

The command runs passes in a new order each (`--shove-passes`, `--shove-patience`) and, for
`Region`, doubles a failed pad's reach in `reaches` each pass up to 4 mm. That loop lives in
`run`'s `shove_pass`, not in the library.

### Fine-pitch parts

`morph::fine_pitch(&board, pitch) -> Vec<usize>`: parts with four or more pads whose closest
pads of different nets on a shared layer sit within `pitch` (the command uses 0.5 mm). Pre-fanout
escapes them, and the rip plough tries only their trapped connections.

## Writing back to KiCad

Three lists make the file: `removed` (spans), `replace` (spans and new text) and the router's
copper.

```rust
let (tracks, vias) = grid::edits(&board, &laid);
let routed = kicad_pcb::apply(&text, &kicad_pcb::Edit { remove: removed, replace, tracks, vias });
```

`edits` writes only `Track` and `Via` items, in metres, with net numbers and layer names from the
board. Items in `laid` that are gone are skipped, so a stale list is harmless; an item in it
that still has a span would be written twice, which is why moved copper always loses its span.

Moved parts go in `replace`, as their new `(at ...)`. Keep each part's starting `at` and write
the ones that changed:

```rust
let original_at: Vec<Point> = board.footprints.iter().map(|f| f.at).collect(); // before any move
// ... route, push, nudge ...
let replace: Vec<(Span, String)> = board.footprints.iter().zip(&original_at)
    .filter(|(f, at)| f.at != **at)
    .filter_map(|(f, _)| {
        let angle = if f.rotation_deg == 0.0 { String::new() } else { format!(" {}", f.rotation_deg) };
        Some((f.at_span?, format!("(at {} {}{angle})", kicad_pcb::mm(f.at.x as f64 * 1e-9), kicad_pcb::mm(f.at.y as f64 * 1e-9))))
    })
    .collect();
```

Pads are written relative to their footprint, so moving the `at` moves them. Pours are written
unchanged; refill them in KiCad (`kicad-cli pcb drc --refill-zones` or the editor) before judging
the board.

## The command's stage order

`ultraroute::run(&args, board, &text, live, cancel)` is the reference pipeline. With no flags
it runs `route_open` and the cleanup only; each flag adds a stage. In order:

1. **Setup.** `--lock` marks parts locked; `--place` moves parts with `move_footprint`
   (copper touching them is ripped); `--strip` removes unlocked tracks and vias (`--strip-all`
   locked ones too); `--planes` calls `set_planes`. Then the solver (`--gpu`), `--adopt`,
   `--via-avoid` boxes and `--rip-region` discs (copper inside ripped).
2. **`--prefanout`**: `prefanout` on `morph::fine_pitch(&board, 500_000)`. Escapes are held apart
   from `laid` unless `--prefanout-soft`.
3. **Planes**: `join_planes` when any layer is a plane.
4. **`--diff-pairs`**: `route_pairs`, held apart until cleanup.
5. **Routing**, once, or `--restart N` more times from the same start with the nets left open
   promoted, keeping the best board:
   `--topo` (`route_topological`, 40% of the time), `--topo-first`, `--first` (priority nets,
   by `route_open` tier by tier or, with `--first-negotiate`, negotiated among themselves),
   `--negotiate` (`route_negotiated`, half the timeout), `route_open` to finish (always),
   `prune_closed_escapes`, `--rip-up` (`rip_up`), `--chase` (`chase`), the escapes join `laid`.
6. **Rescue**: `--shove` (`shove_escape` passes); `--topo-trapped` or
   `--topo-trapped-rip` on what is still open; `--nudge` (`nudge`).
7. **Finish**: `--tighten` (`pull_tight`); pair copper joins `laid`; `drop_duplicates`;
   `prune_dangling`; with `--diff-pairs`, `match_lengths`; `--blame` prints what blocks each open
   connection.
8. **Write**: moved parts' `(at ...)`, `edits`, `apply`.

The time limit (`--timeout`) is shared out: negotiation gets half, the stages after it what is
left. A library pipeline sets each stage's `max_seconds`
itself. Two habits from `run` are worth copying. Hold copper you do not want ripped (escapes,
pairs) out of `soft` until the end. And try any move that might make things worse on a clone:

```rust
let before = connectivity_without_pours(&board).airlines.len();
let snapshot = (board.clone(), laid.clone(), removed.len());
// ... morph, rip_broken, route_open ...
if connectivity_without_pours(&board).airlines.len() > before {
    (board, laid) = (snapshot.0, snapshot.1);
    removed.truncate(snapshot.2);
}
```

## The command as the simplest integration

If the stages above as the command runs them are what you want, run the command:

    ultraroute route IN.kicad_pcb -o OUT.kicad_pcb --strip --timeout 60

With no stage named it runs the default recipe, `ultraroute::RECIPE`; `--recipe minimal` runs only
the grid router.

The `.kicad_pro` (and `.kicad_dru`) beside the input give the rules. Progress goes to standard
error. `ultraroute route` with no input prints every flag.

`--live -` streams the run to standard output as newline-delimited JSON, the studio's format: a
`board` header with every item, then `frame`s with what was added or removed and the router's
activity, `fill` with the zone fills, and a final `done` with the open count and whether it
stopped on `Cancelled` or `Deadline`. Coordinates are millimetres with y up. A line `stop` on
standard input stops the run at the next connection, and the board so far is still written.
`--live PATH` writes the stream to a file; `--live-fps` (30) and `--live-step MS` pace it.

From Rust, the same is `ultraroute::parse(&args)` then `ultraroute::run(&a, board, &text,
live, cancel)`, with `Live::new(writer, fps, pours_join)` for a stream. `Live` cannot be driven
from outside `run`: its frame methods are private.

## WebAssembly

`core/crates/web` builds the whole command for a browser (`crate-type = ["cdylib", "rlib"]`,
`ultraroute` without the `gpu` feature). The page's Web Worker calls
`route(pcb, pro, dru, options, fps, on_line)`: the board's text, the `.kicad_pro` text, the
`.kicad_dru` text or empty, the command's flags as one string, and a JavaScript function called
with every live frame. It returns the routed board's text. `route_accelerated` adds a batch
size and a dispatch function so negotiation's rounds go out to a pool of workers, which run
`set_round`, `route_priced_job` or `route_packed_job` on their own copies of the module.
`last_panic()` returns the message of a panic, which WebAssembly reports only as
`unreachable`. A run cannot be stopped from inside; the page terminates the worker.

## Routing regions and length limits

`--regions FILE` (or `board.regions` from a program) takes a planner's areas with rules for which
nets may run there: circuit-composer's `routing.json`, version 1. Each region is a polygon in mm
(KiCad's frame), its copper layers (all by default) and rules: `never` (keep out), `must` (stay
inside, on the region's layers), `avoid` / `prefer` with a `weight` (a track pays `weight` times
its length inside, or outside; a via pays `weight` mm of track), for tracks or, with `"vias":
true`, for vias. A rule holds for the nets and classes it names, `"*"` for all. The board keeps
them as `board::Region`; each obstacle map rasterises them once (`grid::regions`) and every net's
view gets the rules that hold for it: hard rules as blocked cells, costs in
`RouteGrid::region_cost` and `via_penalty`, which the CPU searches add to every step (the GPU
and the studio's worker pool do not see the costs yet). `max_lengths` entries
(`{"nets": [...], "max_length_mm": N}`) are checked and reported at the end, with copper that
breaks a region's hard rule, by region name; they are not yet routed to.

