# lyv — design notes

How lyv works and why. Figures come from real builds: 35k NZ building outlines,
368k cadastral parcels, and 3.9M US floodplain polygons. [speed.md](speed.md) and
[memory.md](memory.md) cover where the time and memory go.

The Python scripts beside this file are the prototypes that settled several of
the decisions below, and are cited where they apply.

## Contents

1. [Approach](#1-approach)
2. [Crates](#2-crates)
3. [The pipeline](#3-the-pipeline)
4. [Coordinates and precision](#4-coordinates-and-precision)
5. [Cutting and closing](#5-cutting-and-closing)
6. [Merging](#6-merging)
7. [Simplification](#7-simplification)
8. [Dropping and budgets](#8-dropping-and-budgets)
9. [Buffers](#9-buffers)
10. [Attributes](#10-attributes)
11. [The tile store](#11-the-tile-store)
12. [Format facts](#12-format-facts)
13. [Testing](#13-testing)
14. [Open questions](#14-open-questions)

---

## 1. Approach

**Bottom-up.** The source is cut once, at maxzoom, and every level above is made
by merging four children into a parent.

A top-down tiler clips full-precision source for every tile at every zoom, which
is O(V × Z). Bottom-up is O(V): each level reads only the level below, already
shrunk by simplification, so if halving the resolution roughly halves the vertex
count the whole pyramid costs `V(1 + ½ + ¼ + …) = 2V`. Where nothing simplifies
away it degrades towards O(V × Z) — no worse than top-down.

It also makes seam points bit-identical between neighbouring tiles for free,
because each boundary crossing is computed once and written into both tiles.

**What it costs: fidelity in the unbalanced case.** A feature dropped at zoom `z`
can't come back at `z-1`, because the source is never re-read. Usually that's
harmless — a parent holds four children's ground at half the resolution, so it's
tighter than they were. But a dense child beside empty siblings gives a parent
that is *roomier*:

```
budget: 500 features' worth
dense child held 1000, kept 500
parent inherits 500 × ½ = 250     -> 50% of budget
the 500 dropped would have fitted
```

Coastlines, cities and road networks all produce this.

**Few flags.** Anything that can be worked out from the data — the maxzoom, what
to drop — is worked out rather than asked for. Things someone arriving from
another tiler might notice:

- **Maxzoom is chosen from the data** when not given, between 12 and 16 (§8).
- **Dropping is uniform across a zoom level, never per tile** (§8).
- **Only EPSG:4326 and EPSG:3857 input** is accepted; both reach the tile grid by
  arithmetic, with no projection database.
- **Self-intersecting polygons whose pieces won't close correctly are dropped**
  and counted (§5).

## 2. Crates

Separate crates rather than modules, because a crate boundary is enforced by the
compiler.

| crate | does |
|---|---|
| `lyv-core` | shared types (`TileCoord`, `Chain`, `Pieces`, `Value`), Web Mercator |
| `lyv-source` | reads FlatGeobuf, GeoParquet, GeoJSON; projects and orients |
| `lyv-cut` | source geometry to per-tile pieces, in one sweep |
| `lyv-close` | rejoins clipped ring pieces along the tile edge |
| `lyv-coverage` | full-tile squares for tiles a polygon swallows whole |
| `lyv-seam` | unions adjacent pieces by cancelling shared edges |
| `lyv-merge` | four children to one parent |
| `lyv-simplify` | Douglas-Peucker and Visvalingam, protecting tile boundaries |
| `lyv-rank` | the drop order, and how many features a level can afford |
| `lyv-buffer` | welds a margin of neighbouring geometry onto a tile |
| `lyv-mvt` | MVT encode and decode |
| `lyv-store` | the SQLite intermediate store |
| `lyv-archive` | PMTiles and MBTiles writers |
| `lyv-progress`, `lyv-alloc` | progress lines; the optional live-bytes counter |
| `lyv` | the pipeline and the CLI |

`lyv-archive` depends on nothing else of ours, and not on `geo`: its whole
contract is four integers and a byte slice.

## 3. The pipeline

1. **Read** each feature, project it onto a global integer grid, and record its
   size and attribute cost. Attributes go to the store.
2. **Choose the maxzoom**, if not given, by cutting at a candidate zoom and
   estimating its worst tile (§8).
3. **At maxzoom**, per feature: simplify, cut into tiles, check the pieces will
   close back to the feature's area, and note which tiles it swallows whole.
   Then close every tile's ring pieces, fill the swallowed tiles, and rank all
   features.
4. **Thin maxzoom** if its worst tile is over budget.
5. **Climb**: for each level up, merge each parent's four children, measure the
   level, and thin it. The thinned level is what the next merge reads.
6. **Encode** every tile with a buffer taken from its eight neighbours, and write
   it straight into the archive — levels in ascending zoom, each in Hilbert
   order, which is the order PMTiles wants.

**No spatial index is needed anywhere.** An index answers "which features are in
this tile?", which a top-down tiler asks for every tile. Bottom-up only asks
"which tiles does this feature go into?", which is arithmetic.

**Parallelism.** Reading is one thread, handing batches to a cutter that uses
every core. The maxzoom store, each merge level and the encode pass run across
threads too. Encoding defaults to at most four workers: past that, concurrent
store reads contend (see `Options::workers` and [speed.md](speed.md)).
**The worker count never changes the output** — checked byte-for-byte at 1, 3, 8
and 64 workers in `crates/lyv/tests/threads.rs`.

## 4. Coordinates and precision

`lyv_core::mercator` is the **only** floating-point step. Degrees (or EPSG:3857
metres) go in; integers on a global grid come out. Cutting, merging and encoding
are all integer arithmetic, so coordinates compare exactly — which the merge
relies on.

- **Global**: `i64`, spanning `extent << z` units across the world at zoom `z`.
- **Tile-local**: `i16` (`Local`), `0..=extent` within a tile.

`i16` is enough because extent is capped at 8192, and the widest value a chain
ever holds is `2 × extent`, in the merge's doubled frame. **Arithmetic widens;
only storage is narrow.** Watch for `i64::from(b.x - a.x)`: the subtraction
happens in `i16` and wraps silently in release builds.

**Precision is `extent × 2^zoom`**, so zoom and extent trade freely. That is what
lets the maxzoom be chosen *after* the read: everything is projected onto the
grid of the deepest zoom the run could pick, then scaled down once the choice is
made.

**Extent must be a power of two**, because moving between levels shifts or halves
coordinates.

**Requantizing at every level is safe.** Each level halves the inherited error
before adding its own half-unit, so the total converges to one tile unit however
many levels cascade — about ⅛ of a pixel at extent 4096. Rounding must be to
nearest: truncation drifts the whole layer by half a unit (`quant.py`).

**Maxzoom uses extent 8192; other levels use 4096.** MapLibre normalises every
tile to 8192 internally (`src/data/extent.ts`: 16-bit vertex buffers, minus three
bits for sign, packing and overflow), so 4096 is scaled ×2 on load and wastes a
bit, while 16384 is scaled ×½ and discards one. Maxzoom is the level a viewer
overzooms past and the one whose detail can't be recovered later, so it gets the
full 8192.

**Features narrower than one unit are skipped and reported.** Every vertex lands
in one column, so the ring encloses nothing. The run says so:

```
  skipped 12 too small to draw at zoom 12
```

`collapsed` and `no_geometry` are counted separately, because a deeper maxzoom
fixes the first and nothing fixes the second.

**Rounding can't reverse a simple ring's winding**, but it can fold a small ring
through itself, leaving something that reads as a hole (`flip.py`). §5 and §6
guard against the consequences.

## 5. Cutting and closing

### Cutting

`lyv-cut` splits each segment at every tile boundary it crosses, then assigns
each sub-segment to a tile **by its midpoint**, which is on a boundary only when
the sub-segment runs along one; it then goes to the tile east or south. O(V + T),
where clipping against every candidate tile is O(V × T).

- **A corner crossing** gives a vertical and a horizontal crossing at the same
  point; the zero-length sub-segment between them is dropped.
- **Crossings round, not truncate**, or every crossing drifts the same way.
- **`cut_ring` splices** the first and last chain when both land in one tile,
  since a ring's first vertex is usually mid-tile.
- **Every step goes the short way.** A step of more than half the world, like
  179.9 to -179.9 or a seam written as 180 then -180, crosses the antimeridian;
  read literally it's a line right round the world. The rest of the line or
  ring is shifted by a world instead, so it runs on past 180 and the wrap below
  puts it in the right column. Without exception, so a ring that goes right
  round the world, like Antarctica's, can't be drawn.
- **Longitude wraps.** Past 180, geometry lands in the column it wraps to, so
  data in the 0–360 convention, or running on past 180 as sources near the
  antimeridian often do, keeps every piece. The coverage fill wraps the same
  way. Latitude doesn't: geometry north or south of the world is dropped.
- **A segment along the world's east edge belongs to the last column**, an
  exception to "east or south". Otherwise a polygon ending at 180, such as one
  half of a feature split there, would leave its edge alone in column 0.
- **A point is never at `extent`.** MapLibre skips circles and symbols at
  `x >= extent` or `y >= extent`, so a point on a tile's east or south edge
  would vanish. The cut puts it at 0 in the next tile (180 wraps to column 0);
  one the merge rounds onto the far edge is pulled back a unit. The southern
  limit has no next row, so a point there sits a unit inside the last.
- **Bounds are wrapped too.** Data past 180 is declared where it wraps to; data
  that really crosses the antimeridian is declared across every longitude,
  since MapLibre clamps a source's bounds to ±180 and fetches no tile outside
  them.

### Closing

A tile gets open chains — the parts of each ring inside it. `lyv-close` rejoins
them into closed rings along the tile edge. This is the second half of
[Weiler–Atherton](https://en.wikipedia.org/wiki/Weiler%E2%80%93Atherton_clipping_algorithm):
from each chain's exit, walk forward along the tile perimeter, emitting the
corners you pass, to the next chain's entry; repeat until back at the start.

Easy to get wrong:

- **The walk must emit corners.** A straight line from exit to entry is only right
  when both lie on the same edge.
- **Closing isn't per chain.** A band crossing a tile joins one chain's exit to
  the *other* chain's entry.
- **The walk direction is fixed**: clockwise on screen, the same sense as an MVT
  exterior. Checked against Sutherland–Hodgman in `close.py` and in
  `crates/close/tests/` over 8,600 tiles.

So **consistent orientation is a precondition**: exteriors clockwise on screen,
holes anticlockwise. `lyv-source` imposes it from the polygon structure rather
than trusting the file. Given that, holes need no special handling — a hole
biting a tile edge simply becomes part of the exterior's boundary.

### Self-intersecting rings

That precondition is checked on a ring's *net* signed area, which a
self-intersecting ring renders meaningless. Such a ring can leave a piece whose
ends sit on the perimeter in a hole's order while the ring is an exterior, and
closing then returns the tile's complement: a full-tile rectangle.

Finding self-intersections would mean an O(V log V) sweep over every ring. So
lyv checks the result instead: **cutting and closing conserve area.** Closing's
artificial edges along a boundary are traversed in opposite directions by the
two tiles sharing them, so they cancel, and a ring's pieces must sum to the
ring's own area. A complement misplaces a whole tile. Legitimate slack is a unit
or two.

The check runs per feature during the cut, while the feature and its pieces are
both in hand: `closing_would_lose_it` measures what closing *would* produce
(`closed_area_within`, the same walk without keeping the rings) and drops the
feature if it's off by more than half a tile:

```
  dropped 2 self-intersecting features
```

A size check alone ("no piece may exceed its tile") isn't enough: the complement
can come out a few units *under* a tile's area as easily as over.
`crates/lyv/tests/tangled_rings.rs` has both cases, from floodplains features
3256752 and 722822.

### Coverage

A tile lying wholly inside a polygon gets no chains, so closing never sees it.
`lyv-coverage` finds these by a scanline along each tile row: coverage can only
change across a tile that contains boundary, so one winding count decides each
run of empty tiles. O(V + T), not point-in-polygon per tile.

Such a tile stores the **tile square as an ordinary ring**, and merges by
cancellation like anything else.

**Coverage is decided per feature, by net winding, emitting at most one square.**
Asking each ring separately would put coincident squares of opposite winding in
a tile wherever rings nest, and a coincident opposite pair on the parent's
boundary is not a seam, so the merge would never cancel it (`coverage.py`).

## 6. Merging

### Cancelling seams

Adjacent pieces of one polygon traverse their shared edge in opposite
directions. Cancel those edges and the survivors bound the union — exact on
integers, with no boolean library and no tolerances.

**Only edges on the two internal seam lines can cancel**, so don't search for
matches:

1. Classify each edge: on `x = mid` or `y = mid`? Two comparisons. Anything else
   survives untouched.
2. Per seam line, run a signed 1-D sweep: each edge is an interval weighted ±1 by
   direction; emit the elementary intervals with non-zero net weight.

The sweep handles partially overlapping collinear edges, which exact matching
wouldn't — they arise when a real boundary runs along a seam for part of its
length (`collinear.py`, `seam.py`). Verified in `cancel.py`, `cancel2.py` and
`holes.py`: every merged area equals a direct clip, and winding comes back right.

**Order inside a merge:** place the children in a frame of extent `2E` (pure
offsets, so exact), cancel seams, *then* halve and simplify. Cancellation must
see unrounded coordinates; simplification must come after cancellation, when
former seam vertices are interior.

**Collinear points are dropped inside the merge.** Otherwise the midpoints of a
full tile's sides double at every level, and they sit on the boundary, where
simplification may not touch them.

**Rejoin by runs, not by point lookup.** Non-seam edges already arrive in order
from the child rings; splice the surviving runs using the seam survivors.

**Lines join only where an endpoint lies on an internal seam.** A line can leave
the parent tile and come back at the same point, which looks like a join and
isn't.

### The shared-edge invariant

Cancellation depends on this: **two tiles sharing an edge hold bit-identical
geometry along it, traversed in opposite directions.** The cut establishes it at
maxzoom. The merge preserves it because halving is deterministic, both sides
close with the same corners, and **simplification never moves or removes a
vertex on the tile boundary**.

That last rule needs no coordination — both tiles hold the same boundary
vertices and both keep them. It's implemented by splitting each ring at its
boundary vertices and simplifying each run separately, since every simplifier
keeps the ends of what it's given. This requires simplifying to a tolerance, not
to a target vertex count.

The protection is temporary: a vertex on a seam becomes interior once its
parent is merged, and is simplified at the next level.

### Collapse

**Dust.** A polygon that shrinks until every vertex rounds onto one point is
replaced by the smallest square the grid can hold, so a layer of small features
thins out instead of vanishing. A collapsed line becomes the shortest segment
the grid can hold, still pointing the way it ran. `--no-dust` turns this off.

A collapsing ring is cancelled by the seam sweep before anything downstream sees
it, so candidates are gathered from the children *before* the merge. Only rings
small enough to be at risk are tracked, and dust is given only when the feature
has nothing else left in the tile. It isn't free: on the NZ buildings with no
budget it took the archive from 1.83 MB to 3.35 MB.

**Rings that invert are dropped.** Rounding can fold a small ring through itself
so it comes back wound as a hole; a renderer then fills around it. Two guards:

- in the merge, a ring whose sign changes on halving is rejected — a similarity
  can't reverse orientation, so a flip means it degenerated;
- after simplification, a hole with no exterior of its own feature left in the
  tile is dropped.

The same pass drops zero-area rings. On the NZ buildings that was 67,214 of
them, and removing them took the archive from 4.60 MB to 3.76 MB.

## 7. Simplification

Load-bearing, not tidying: halving coordinates doesn't reduce the vertex count.
Without simplification a parent stays 4× a child; with it, 2×.

**The tolerance names its algorithm.** `--simplify-distance` is Douglas-Peucker's
distance from the chord; `--simplify-area` is Visvalingam's triangle area. They
can't be converted — a vertex `d` from a chord of length `b` has area `b·d/2` —
so `Tolerance` is an enum and the algorithm follows from the unit.

**The default is `--simplify-distance 1`**, one tile unit: detail finer than that
is already unrepresentable on the grid. Visvalingam would suit a bottom-up
pyramid better, since it's nearly nested (simplifying to N then N/2 ≈ straight
to N/2) and Douglas-Peucker isn't. See §14.

**Maxzoom is simplified before it's cut**, once per feature rather than once per
clipped piece. On floodplains that discards about 61% of the vertices the cut
would otherwise have built.

**Seam crossings go by ordinary collinearity.** A crossing is interpolated on a
straight segment, so once merging makes it interior, any tolerance above a unit
removes it. On a NZ coastline at z0, one tile unit took 111 points to 28.

### Shared borders

Two features sharing a border can simplify it differently, opening gaps. Over
3,000 synthetic shared borders, 5% disagreed at a light tolerance and 40% at a
heavy one.

The cause is that the two features hand the algorithm different sequences that
merely overlap. Two fixes:

- **Done:** simplify every run in a canonical direction, since both algorithms
  break ties by iteration order and the two features traverse the border in
  opposite directions. This alone takes direction-induced disagreement to zero.
- **Not done:** find the shared runs (an edge hash across a tile's features) and
  split at their ends, so both features simplify the identical sequence. Tested
  in isolation, this took 2,910 disagreements to under 21. Not built, because
  real datasets look fine without it.

## 8. Dropping and budgets

### Uniform per level

Vary the drop rate per tile and features vanish mid-span at tile edges — tilers
that offer per-tile limits warn about exactly this. So every tile at a zoom uses
the same cut-off.

That cut-off is a prefix of one **total order over all features**, computed once.
A shorter prefix is contained in a longer one, so a feature kept at zoom `z` is
kept at every deeper zoom for free.

### The rank: evenstar

1. Within each tile, order its features by a weighted lottery (below).
2. Visit tiles in a pseudo-random order, taking each tile's next unranked
   feature. Repeat until everything is ranked.

**Every round takes one feature from every tile that has any left**, so no region
is emptied. Sorting globally by area does empty regions — anywhere holding only
small features never reaches a global top-N. On the NZ buildings at one feature
per tile, a global area sort left 70 of 86 tiles blank; evenstar left none.
`--ranker by-area` is kept for comparison.

**Big features rise anyway.** A feature spanning `k` tiles gets `k` chances to
head a tile in round 1, so its expected rank is about `(T+1)/(k+1)`. In
simulation every multi-tile feature lands in round 1 (`rank_sim.py`).

**The lottery** is an exponential race: key `E / w` with `E = -ln(U)` from a seeded
hash, taken ascending, gives `P(first) = w / Σw` exactly (Efraimidis and
Spirakis, 2006). Weight is size squared, so a feature twice the size wins 80% of the
time rather than always. Polygons, lines and points take turns, since their sizes
can't be compared.

**Bucketed four levels below maxzoom** (`--rank-depth 4`). Buckets at maxzoom
itself would make the order within one maxzoom tile pure lottery by size, and
maxzoom gets thinned on real data — on the parcels, a third of the dataset was
decided there. `lyv_cut::for_each_tile` reports which deeper tiles a feature
touches without building chains. Coverage isn't filled at that depth, which
slightly under-counts the span of huge polygons.

**Tile order is a hash of the tile coordinate**, not a shuffle, so adding a tile
wouldn't reorder all the others.

### The budget

After a level is merged: find its worst tile by estimated bytes and by features.
If either is `f` times over, narrow the keep-count to `1/f` and rewrite the level.
The next merge reads the thinned level, so the corrections compound on their own,
and `narrow` never widens, so monotonicity holds. Maxzoom is thinned too, since
it has no deeper level to be corrected from.

One application under-corrects — the ranking keeps large features, so dropping
half the features removes less than half the bytes — and the next level up
catches the rest.

**Bytes are estimated, not measured.** The threshold has to be chosen before
encoding, and stored geometry alone under-read real tiles by 15–30× on the
parcels, because attributes dominate. So a tile is estimated as its geometry plus
each distinct feature's attribute cost, computed once at read time by
`lyv_mvt::attribute_cost` beside the encoder. Keys aren't charged, since a layer's
keys pool almost perfectly. Values are charged per feature, so the estimate
over-reads by about 1.5× — the safe direction.

Encoded tiles gzip by about 2×, not 4×. The default `--max-tile-bytes 4000000` is
roughly 1.3 MB gzipped: well over the conventional 500 KB, deliberately, because
dropping real features to meet a bandwidth number from an older era is the worse
trade.

### The vertex budget

Bytes don't measure what a renderer pays for; vertices do. A 2 MB tile of
attributes renders fine and a 2 MB tile of river geometry doesn't. Bytes per
vertex vary 2× between layers (5.8 on floodplains, 10.2 on parcels), so a byte
limit can't stand in.

And the remedy is different: too many vertices is the same features described
too finely, so the fix is to **simplify harder**, not to drop. Dropping one
floodplain feature can remove 24,000 vertices and a whole river.

- `--max-tile-vertices`, default 200,000. Measured: a 267k-vertex floodplains tile
  was judged fine and 324k too heavy.
- **Trigger:** the level's p90 is over the budget, or any tile is over twice it.
  Not the mean — on floodplains the mean *rises* up the pyramid, from 19k at z10
  to 197k at z2, while the heavy tiles first appear at z8.
- **Correction:** a finished level can't be rebuilt, so the tolerance for the
  levels *above* it is coarsened by one level's worth (distance ×2, area ×4).
  The lead never decreases, and is capped at 3.
- **Measured as shipped:** the encode pass adds the buffer after the merge
  measures a tile, so the measurement is scaled by `(1 + 2·buffer/4096)²`, about
  6%. That 6% was the difference between z8 on floodplains firing or not.
- Doubling the tolerance halved a heavy floodplains tile (491k to 249k vertices).
  Below about 63k vertices no tolerance helps, because the remaining features have
  no interior vertices to remove. So `--max-tile-features` defaults to 50,000, a
  quarter of the vertex budget.

### Choosing the maxzoom

**Too shallow a maxzoom looks like a dropping problem.** At z12, a tile over
Atlanta holds 40k parcels at ~400 bytes of attributes each; the correct answer to
that isn't to drop 97% of them but to go deeper.

The probe cuts at `--min-auto-zoom` (12), estimates the worst tile, and if it's
`f` times over, jumps about `log4(f)` levels deeper — a level down quarters a
tile's features. On the parcels:

```
z12 would hold    42 tiles, worst about 38,636 KB / 49,011 features, 39.6x over
z15 would hold 1,507 tiles, worst about  3,762 KB /  4,382 features,  3.9x over
z16 would hold 5,631 tiles, worst about  1,822 KB /  2,114 features,  1.9x over
capped at z16; the rest comes off by dropping features
```

Told `-z 12`, that layer kept 3% of its features; left to choose, 55%.

`crates/lyv/tests/auto_maxzoom.rs` checks that choosing a zoom builds exactly what asking
for it builds.

**The floor, 12, is usually the answer**, and it's the only thing defending
resolution: the budget asks whether a tile is too big, never whether it's too
coarse. At z12 with extent 8192 a unit is about 1 m at mid latitudes.

**The cap, 16, is about tile count.** At z16 a unit is already 2.6 cm, finer
than cadastral survey accuracy — but more to the point, NZ land alone is 717k
tiles at z16 and 11.5M at z18, and the climb holds a whole level in memory.

**`extent_at` must not be asked before the maxzoom is chosen.** It compares
against `deepest()`, which is the cap until the choice settles, and asked early it
doubles every tile address — silently, with `pmtiles verify` passing.

## 9. Buffers

Internal tiles are clipped exactly, because cancellation needs pieces to abut.
So output buffers are built at encode time from the eight neighbours — the one
pass that isn't local to a tile.

**Copying the neighbours isn't enough for polygons.** Each neighbour's clipped
ring carries an artificial edge along the shared boundary, so a stroke would draw
the tile grid inside the fill. The margin is **welded** with the same edge
cancellation the merge uses, across the tile's four edges. What's left is the
real boundary plus runs along the margin box, outside the tile, where MapLibre's
per-tile stencil clip discards them.

- Each neighbour's pieces are closed **separately** against the margin box. Closed
  together, the perimeter walk pairs one neighbour's exit with another's entry.
- **A neighbour's ring can't bring in more area than it had**, since clipping is
  an intersection. It's checked (`within_its_means`): a parcel next door once
  arrived as the complement of a two-unit sliver, painting over a whole z7 tile.
- **Only polygons are buffered.** Two line pieces meeting at a tile edge already
  draw as one. The cost is a notch at a sharp bend within half a stroke width of
  the edge; buffering lines would bring dash-phase and double-draw artefacts
  instead.
- `--buffer` is in 4096ths of a tile, default 64, so it's the same fraction of a
  tile at maxzoom's 8192 extent. Zero skips the neighbour reads entirely.
- Longitude wraps (the tile east of the last column is the first); latitude
  doesn't.

**Why not buffer internally, or extrude?** A child's buffer is half as wide in
its parent's frame, so buffers halve at every merge: reaching z0 with 64 units
would need `64 × 2^14` at maxzoom 14. Extruding the clipped edge outward does
nothing for lines, which have no artificial edge.

**The limit.** MVT can't mark an edge as artificial, so a stroke wider than twice
the buffer shows the grid. At 64 units in 4096 that's 8 px outside the edge of a
512 px tile, so strokes over about 16 px. The real fix is a separate outline
layer of lines, as basemaps ship for boundaries; lyv doesn't emit one.

## 10. Attributes

Attributes belong to a feature, not to its pieces: a feature cut into three
pieces in one tile is one MVT feature with a multi-part geometry.

**The pyramid carries none.** They're stored once per feature, in the store's
`features` table, and attached only at encode time. Each row is compact binary
against a shared key `Schema` — a varint key index, a one-byte type tag, and the
payload — which grows as new keys appear, since GeoJSON has no header. The encode
pass loads the whole table into one flat arena. `lyv-store`'s `dump_attributes`
example reads it back.

Values are sorted by key on the way in, so the same data in FlatGeobuf and GeoJSON
produces byte-identical tiles.

Mapping to MVT:

- a number is an integer if it is one and fits, a double otherwise;
- `null` is **dropped**: MVT has no null, and an empty string would make
  `["has", "name"]` true;
- arrays and objects become their JSON text.

## 11. The tile store

A SQLite file beside the output (`.lyv-store`), deleted when a run succeeds and
kept when it fails or with `--keep-store`. Every run starts it empty: every
stored tile is encoded, so a leftover store would leak its tiles into the next
archive.

**Why not loose z/x/y files:** block rounding. Most tiles are a few hundred bytes;
at 300 B a tile, 4 KiB blocks use 13.7× the space. A maxzoom-13 pyramid is also
89.5M inodes, more than a default 1 TB ext4 has.

**Why not PMTiles:** it can't be edited or appended.

Schema:

- `tiles (z, tile_id, y, x, data)`, keyed by Hilbert id, so each level
  comes back in PMTiles order.
- `features (id, attrs)` — attributes (§10).
- `ranks (id, rank, attr_bytes)` — the drop order and each feature's attribute
  cost.
- `job (key, value)` — the attribute schema, so a kept store can be read on its
  own.

Tables are `WITHOUT ROWID`, so a lookup is one B-tree probe. Tile blobs are **uncompressed MVT**, so
any MVT tool can read the intermediate pyramid.

**Writes are batched** in ten thousands. One commit per row made writing
attributes the entire cost of reading: 12.8k rows/s against 219k batched.
`synchronous` is `NORMAL`: the file is scratch, so surviving a power cut
doesn't matter.

## 12. Format facts

### MVT

- **Winding is the only exterior/hole flag.** A wrongly wound exterior is a hole
  with no polygon. Tile space has y down, so the sign is the opposite of GeoJSON's
  right-hand rule.
- **Keys and values are pooled per layer**, not per feature.
- **`LineString` and `MultiLineString` encode identically**; "multi" is inferred
  from the number of `MoveTo`s.
- **Geometry past the extent is legal**, which is how buffers work (spec §4.1).
- **Feature ids need only be unique within a layer** (spec §4.2) — nothing about
  stability across tiles.
- **MapLibre fills closed LineStrings.** Its fill bucket classifies rings by
  signed area without checking the geometry type. Don't rely on it: every other
  consumer sees lines.

### MBTiles

- Rows are TMS (from the south); lyv's store and API are XYZ, flipped in the
  writer.
- Vector tile blobs must be gzipped.
- No deduplication in the plain schema — a lake's millions of identical covered
  tiles are each written in full. The `map`/`images` schema would fix that; not
  done.

### PMTiles

- `vector_layers` in the metadata JSON is how a consumer learns a layer exists.
  TileJSON has three field types, so numeric widths become `Number`, and a key
  seen with two types is declared `String`.
- Bounds and centre are fixed header fields (int32 × 1e7 degrees at offsets 102,
  106, 110, 114; centre zoom at 118; centre at 119 and 123). Unset, they claim the
  whole world.
- Identical tiles are deduplicated and consecutive runs run-length encoded. A fully
  covered tile is the same 38 bytes at every zoom, so a disc of 3M covered z14
  tiles becomes 1,969 directory entries.
- `clustered` needs tiles written in Hilbert id order, which is ascending zoom,
  then Hilbert within a level.
- An unbalanced pyramid is storable but not announceable: nothing marks a tile as
  having no children, and `pmtiles.js` renders a missing tile as empty rather than
  falling back to its parent. So lyv writes a balanced pyramid.

## 13. Testing

**Test at the seams, against the narrowest interface.** `TileWriter` takes four
integers and a byte slice; testing against the trait rather than either format
found MBTiles panicking where PMTiles returned an error.

**Verify with independent decoders** — `pmtiles verify` and `tippecanoe-decode`,
skipped when absent. `tippecanoe-decode` reports malformed geometry on stderr
while exiting zero, so the test asserts stderr is empty. The same payload written
through both archive formats must come back byte-identical.

**Property-test the sweeps, then mutation-test the properties.** A surviving
mutation is as often dead code as a missing test: a dedup pass in `cut`, a
single-part branch in `encode_chains` and an unreachable error variant were all
found this way.

- Reassembly is ambiguous for self-intersecting geometry, so arbitrary inputs get
  invariants (length preserved, nothing far from the original) rather than exact
  reassembly.
- Halving is accurate to a unit, so compare only what the pyramid actually
  determines.
- **Assert signs and counts, not just areas.** Every bug in the collinear-overlap
  work had the right area and the wrong ring count or winding.

**Builds are byte-reproducible** across runs and worker counts, so an A/B can
compare archives directly — written to the same basename, since the name is in
the metadata.

## 14. Open questions

- **Visvalingam or Douglas-Peucker as the default.** §7 argues Visvalingam;
  the default is Douglas-Peucker because the CLI defaults to a distance.
- **Shared borders** (§7): detecting shared runs is understood but not built.
- **Large flat polygons cost O(area).** A lake covers huge numbers of maxzoom
  tiles, and each is carried through every merge. The cut itself is
  O(perimeter), and PMTiles dedup makes the output nearly free, so this is CPU and
  intermediate storage only. Leads: keep coverage as runs rather than tiles, or
  re-derive it per level from boundary tiles.
- **The vertex budget** is flat across zooms; perhaps it should scale.
- **Where the attribute table stops scaling.** It's held in memory for the encode
  pass. On an attribute-heavy layer its load already comes within 10% of the
  run's peak ([memory.md](memory.md)).
- **Attribute accumulation** (summing or averaging as features merge) isn't
  designed. A mean of means isn't a mean.
