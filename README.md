<img src="docs/lyv.png" alt="" height="96" align="right">
<br>

# _`lyv`_

**LYV your vectors.** A vector tile builder: give it a layer, get a tile
pyramid.

```
./build.sh
./lyv parcels.fgb parcels.pmtiles
./view.sh parcels.pmtiles
```

Reads FlatGeobuf, GeoParquet and GeoJSON; writes PMTiles or MBTiles.
_`lyv`_ doesn't generally need tuning with flags: 
- automatically picks the maxzoom
- automatically picks the geometry simplification and features to drop
- only drops features, never merges them

You might think that you would need to merge features in order to see your
dataset once you've zoomed out too far to see the individual features.
But _`lyv`_ keeps enough features that you can always see the shape of your
dataset, so never merging just avoids a whole class of problems around
which features are allowed to be merged.

## Usage

```
lyv input.fgb output.pmtiles
```

That is the whole thing for most inputs. The layer in the tiles gets the same
name as the output file, so this one would be called `output`.

To name the layer something other than the output file:

```
lyv water.fgb hydro.pmtiles --output-layer hydrography
```

To carry only the attributes a style will use, which is the largest single
thing you can do about tile size on an attribute-heavy layer:

```
lyv parcels.fgb parcels.pmtiles --attributes title_no,owner,area
lyv parcels.fgb parcels.pmtiles --skip-attributes notes,audit_id
lyv parcels.fgb parcels.pmtiles --no-attributes
```

An attribute named there that **no** feature turns out to have is an error, so
a typo cannot quietly produce a tileset missing a field, or keeping one. A name
that only some features have is fine.

Every flag is in [FLAGS.md](FLAGS.md), and `lyv --help` gives the reasoning
behind each default.

### Coordinate reference systems

Input must be in **EPSG:4326** (lon/lat in degrees) or **EPSG:3857** (Web
Mercator in metres). Anything else needs to be reprojected first.

A source that declares **no** CRS is read as lon/lat.

## Why it is fast

Tiles are built **bottom-up**. The source is cut once, at maxzoom, and every
level above is made by merging four tiles into one. As tiles are simplified,
that simplification means less data that the next zoom level needs to work on.

It's hard to give a completely defensible comparison versus tippecanoe,
since it will depend on the machine, the dataset, and the flags chosen.
So, I've included only anecdotal numbers - how they both fared on a
particular 1.3GiB cadastral layer:

| | tippecanoe 2.45.0 | lyv | |
|---|---|---|---|
| wall | 6m 40s | **39s** | **10x faster** |
| CPU | 830s | **97s** | **8.5x less** |
| instructions | 7.2 T | **1.0 T** | **7x fewer** |
| peak RSS | 3.1 GB | **1.9 GB** | **1.6x less** |

Resident set is the least flattering way to measure lyv and it is the one in
the table, because it is the one both tools can be held to. The same run
reports **881 MB** of bytes actually allocated and live; the rest is memory the
allocator has kept rather than returned.

## Dropping features: _evenstar_

When a tile is over budget, something has to go. Sorting by area erases whole
regions — a city of small parcels loses all of them to one lake on the other
side of the country. So instead every tile takes a turn: one feature from each,
then one more from each, and so on, until the budget is met - this ranking is
given the name _evenstar_.

Within a tile the choice is a lottery weighted by the square of each feature's
size, so large features usually win, but don't always. For comparison,
`--ranker by-area` keeps the plain global sort.

## The name

_**L**YV **Y**our **V**ectors_, or it could be _**L**oad **Y**our **V**ectors_ 
if you have outgrown recursive acronyms long ago. Some say that the only
reason this tiler is named _`lyv`_ is so it has the full name Lyv Tiler.

## Where things are

- `FLAGS.md` — every flag, in a table.
- `docs/design-notes.md` — how it works and why, with the open questions marked.
- `docs/speed.md` — where the wall clock goes, stage by stage, on a real build.
- `docs/memory.md` — where the memory goes, stage by stage, on a real build.
- `crates/` — one crate per idea: `cut`, `merge`, `seam`, `simplify`, `rank`,
  `buffer`, `store`, `archive`.

## Building

`./build.sh` gets you the fast binary and links it to `./lyv`: mimalloc is the
default allocator and is probably 15% faster. Arguments pass through to cargo,
so `./build.sh --no-default-features` gives the system allocator back, and
`./build.sh --features memory-stats` reports live bytes at the end.

## License

BSD-3-Clause — see [LICENSE.txt](LICENSE.txt). The licenses of everything `lyv` links
are in [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md); all of them are permissive
and none is copyleft.
