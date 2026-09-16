# viewer

A local map for looking at a tileset.

```sh
./view.sh out.pmtiles
```

Serves the archive on `http://127.0.0.1:8787/` and opens a browser. Needs only
the Python standard library.

```
--layer NAME   source-layer to style (default: the first one the archive declares)
--port N       default 8787
--no-open      don't launch a browser
--verbose      log every request
```

## What you see

- **Colour by feature id**, cycling through 13 hues. The count is prime so that
  data repeating a geometry (six measurements on one river segment, with
  consecutive ids) doesn't collapse to a couple of colours.
- **`?attr=NAME`** colours by an attribute instead.
- **Fill, outline and vertices.** Vertices are off unless the data is points;
  turn them on to tell simplification from cutting.
- **Tile boundaries**, on by default — seams, buffers and coverage squares are
  invisible without them.
- **The geometry types actually present** in the loaded tiles.
- **No basemap**, so an empty tile looks empty.

Click a feature for its id, type and properties. **fit to data** re-frames the
map. The map opens on the archive's `antimeridian_adjusted_bounds` if it has
them, since data across the antimeridian has bounds claiming every longitude,
and on its bounds otherwise.

There's no location hash: it survives a reload and overrides the framing, so the
map would open wherever you last were rather than on the data.

## Why not `python -m http.server`

It ignores `Range` requests, and `pmtiles.js` reads the archive by byte range.
`serve.py` answers them properly.
