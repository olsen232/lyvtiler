# Flags

Every flag `lyv` takes. `lyv --help` prints the same list with the full
reasoning behind each default; this is the table you scan.

Nothing here is required. `lyv input.fgb output.pmtiles` picks the maxzoom, the
simplification and what to drop by itself.

## Input and output

| flag | default | what it does |
|---|---|---|
| `<INPUT>` | — | Source file. FlatGeobuf for `.fgb`, GeoParquet for `.parquet`, GeoJSON for anything else. |
| `<OUTPUT>` | — | Tileset to write. `.pmtiles` or `.mbtiles`, chosen by extension. |
| `-l`, `--output-layer <NAME>` | the output file's stem | Layer name inside the tiles. |
| `-n`, `--output-name <NAME>` | the output file's stem | Human-readable name for the tileset, in the archive's metadata. |
| `--attributes <A,B,C>` | all | Carry only these attributes into the tiles. Comma separated list. |
| `--skip-attributes <A,B,C>` | none | Carry every attribute except these. Can't be combined with `--attributes`. |
| `--no-attributes` | off | Carry no attributes at all: geometry only. |
| `--geometry-field <NAME>` | the primary column | Which geometry column to read, for GeoParquet with more than one. |
| `--input-crs <CRS>` | what the file declares | What the coordinates are: `EPSG:4326` (or `OGC:CRS84`) or `EPSG:3857`. Nothing is reprojected. |

## Running

| flag | default | what it does |
|---|---|---|
| `-q`, `--quiet` | off | Output one line per stage instead of rewriting a line continuously to show progress updates. |
| `--silent` | off | Output nothing at all on stderr. |
| `-j`, `--threads <N>` | cores | How many threads to use. Zero chooses for you. |
| `--store <PATH>` | beside the output | Where to keep the intermediate pyramid. |
| `--keep-store` | off | Leave the store behind instead of deleting it on success. |
| `--mmap <GB>` | 0 (off) | How much of the intermediate store to memory-map. |
| `--memory` | off | Show memory use on every progress line. Needs a binary built with `--features memory-stats`. |
| `--trace` | off | Break each timing stage into even smaller sub-stages. |
| `-h`, `--help` | — | Print every flag, one line each. |
| `--help-full` | — | Print every flag with the reasoning behind its default. |
| `-V`, `--version` | — | Print the version and exit. |

## Zoom range

| flag | default | what it does |
|---|---|---|
| `--min-zoom <Z>` | `0` | The shallowest zoom to build. |
| `-z`, `--max-zoom <Z>` | chosen | The deepest zoom to build. Chosen from the data when not set. |
| `--min-auto-zoom <Z>` | `12` | The shallowest maxzoom that automatic selection is allowed to select. |
| `--max-auto-zoom <Z>` | `16` | The deepest maxzoom that automatic selection is allowed to select. |

## Geometry

| flag | default | what it does |
|---|---|---|
| `--extent <N>` | `4096` | Tile grid resolution for all zooms except maxzoom. Must be a power of two. |
| `--maxzoom-extent <N>` | `8192` | Tile grid resolution at maxzoom. Must be a power-of-two, must be at least `--extent`. |
| `--no-dust` | off | Drop features that shrink to nothing, instead of keeping the smallest mark the grid can hold. |
| `--buffer <N>` | `64` | Margin of polygon overlap into neighbouring tiles, in 4096ths of a tile. |
| `--simplify-distance <D>` | `1.0` | Simplify using Douglas-Peucker: discard vertices closer than `D` tile units to the chord through their neighbours. |
| `--simplify-area <A>` | — | Simplify using Visvalingam: discard vertices whose triangle with their neighbours is smaller than `A` square tile units. |

## Tile budgets

Three limits, and they are spent differently. Bytes and features are paid by
**dropping features**; vertices are paid by **simplifying harder**, because too
much geometry is not too many things — it is the same things described too
finely. Zero disables any of them.

| flag | default | what it does |
|---|---|---|
| `--ranker <NAME>` | `evenstar` | The algorithm that decides which features are dropped. `evenstar` or `by-area`. |
| `--rank-depth <N>` | `4` | How many levels below maxzoom to bucket features into when ranking them for dropping. Zero buckets at maxzoom itself. |
| `--max-tile-bytes <N>` | `4000000` | Tile size limit, in estimated uncompressed bytes, before features are dropped to make it fit. Zero means no limit. |
| `--max-tile-vertices <N>` | `200000` | Tile size limit in vertices before feature geometry is simplified more aggressively. Zero means no limit. |
| `--max-tile-features <N>` | `50000` | Tile size limit in features before features are dropped to make it fit. Zero means no limit. |

