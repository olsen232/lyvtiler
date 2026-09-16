# Where the time goes

Measured with `--trace`, which times each store call and each piece of CPU work
and prints the split at the end of a run. It's off by default; on, it costs two
`Instant::now()` calls and two atomic adds per measured call.

The figures are **summed thread-time**, not wall clock. Encoding runs a worker
per thread, so its rows add up to far more than the stage took. Compare shares
within a stage rather than totals.

[memory.md](memory.md) does the same for bytes.

## Floodplains: 3.9M features, 16.2 GB in, 2.1 GB out

    wrote floodplains.pmtiles in 2m 53s

     29% reading    3,941,426 features   58.2s
     44% ranking    3,941,426 features   10.3s
     45% store z12  77,898 tiles          6.0s
     50% merge z11 6.5s, thin 5.4s, write 7.5s
     ... the rest of the climb, about 18s in total ...
     65% attributes                       1.5s
    100% encoding   110,140 tiles        49.0s

    reading         cpu: parse + project     3,941,425 calls   40.0s  18.1%
    reading         store: put_feature       3,941,426 calls    6.4s   2.9%
    reading         cpu: cut (parallel)            476 calls   49.3s  22.3%
    reading         wait: reader on cutter         476 calls    5.0s   2.2%
    ingest + climb  store: put tile            110,140 calls    7.8s   3.5%
    encoding        store: get tile            212,825 calls    9.7s   4.4%
    encoding        store: features()                1 calls    1.5s   0.7%
    encoding        cpu: add margin            110,126 calls   42.5s  19.2%
    encoding        cpu: encode mvt            110,140 calls   40.3s  18.2%
    encoding        cpu: gzip + archive          1,379 calls   16.9s   7.6%
    ranking         store: ranks in/out              2 calls    2.2s   1.0%
    all             in the store                              27.6s  12.5%

**It's CPU now, spread evenly.** Parse, cut, margin and MVT encode are 40–49 s
each, with no single item over 23%. The store is 12.5% of thread-time.

**Reading and cutting overlap.** The reader hands batches to a cutter thread, and
the reader barely waits (5 s over 476 hand-offs), so the serial parse is the
critical path.

## How it got here

| change | wall |
|---|---|
| start | 4m 26s |
| SQLite configured for what the store is | 4m 05s |
| reading and cutting overlap instead of taking turns | 3m 10s |
| the attribute table decoded once, not per tile | 3m 08s (−14% CPU) |
| the margin stopped copying neighbours it then threw away | 2m 57s |
| attributes stored against a schema, not as JSON | 2m 53s |
| tile coordinates went from 32 bits to 16 | 2m 54s (memory −38%) |

Each was measured before it was built.

**The margin fix:** `add margin` was 36% of instrumented time. Every ring of all
eight neighbours was shifted into a new vector before being clipped, though only
a thin strip along the shared edge can reach the margin; a bounding-box test now
rejects the rest. And the neighbour cache deep-cloned a whole tile on each of
its nine lookups per tile; it hands out an `Rc` now. 99.9 s → 41.2 s.

**The schema:** stored attributes were JSON, about 63% of it key names, type tags
and punctuation repeated on every row. With the key names interned once in a
`Schema`, a row is a varint index, a type byte and the payload. That halved
`put_feature` and cut the load by three quarters, and the parcels' stored table
went from 530 MB to 219 MB.

## Fastest configuration: 2m 24s

    cargo build --release -p lyv     # without memory-stats
    lyv floodplains.fgb out.pmtiles --threads 10

Almost all of it is encoding, 50.7 s → 31.8 s. CPU went up only 16 s to save 19 s
of wall: 0.85 CPU-seconds per wall-second saved.

## The encode worker cap

Encoding defaults to four workers. The reason is throughput per core, not speed:
many jobs share one machine, so the question is how many fit. Ten workers have
always been faster on the clock, but at a cost in CPU that has fallen with each
change to the encode path: 4.4 CPU-seconds per wall-second saved, then 2.95,
then 0.85.

At 0.85 the cap may no longer be right. The run that measured it also varied
`--mmap`, so it doesn't settle it. The A/B that would: `--mmap 0` at four workers
against ten, on an idle machine, with CPU from `/usr/bin/time`. Read the whole
comment on `Options::workers` before changing it.

## SQLite settings

The store is a scratch file of large blobs, deleted on success; SQLite's defaults
suit a long-lived database of small rows.

| | before | after |
|---|---|---|
| store: get tile | 20.9 s @ 168 MB/s | 8.9 s @ 396 MB/s |
| store: put tile | 12.2 s | 8.0 s |
| encoding, wall | 93.7 s | 76.1 s |
| the run, wall | 4m 26s | 4m 05s |

**`page_size` did it.** Tiles average 16.5 KB, so at 4 KB pages every read chased
overflow pages; at 64 KB a tile is one page. The attribute rows gained least,
because a 64 KB page is no help to a small row.

### `--mmap`

Off by default.

- **Parcels:** nothing. 39.0 s against 39.2 s, identical instruction counts, and
  0.66 GB more resident set (2.56 GB against 1.90).
- **Floodplains:** tile reads about 25% faster, encoding 47.8 s against 50.7 s —
  about 3 s of the run.

The mapping is per connection, and encoding opens one per worker. At ten workers
floodplains' resident set went from 7.1 GB to 12.5 GB with no change in footprint,
and the machine started evicting.

Two percent on one layer and nothing on the other, against a description that
takes three sentences and undoes itself twice:

- the extra memory makes things go faster;
- but it is not really memory, because the kernel can reclaim it;
- but if it keeps being reclaimed, things run slower again.

All three are true. But container limits, dashboards and anyone sizing a machine
read resident set and believe only the first.

## Ruled out

- **Encoding while the climb runs.** PMTiles ids ascend with zoom, so z0 has to be
  written first, and the climb produces z0 last. And encoding is CPU-bound, so it
  would compete with the climb for the same cores.
- **Parallelising the FlatGeobuf parse.** The cutter already takes 45 s, so even a
  free parse saves at most about 11 s, for a large change.

## Caveats

- **`memory-stats` costs about 10% of `reading`** — a thread-local access per
  allocation — so every figure above except the 2m 24s run is a few percent high.
- **Builds are byte-reproducible**, across runs and worker counts — checked on
  `example.fgb` (repeat runs, `--threads 1` and `--threads 10`) and the NZ crop.
  So archives can be compared directly, as long as they're written to the same
  basename: PMTiles metadata includes the output filename.
