# idcoords config — the write-path benchmark

One topic that keeps **every node in the file** with no tags, no producers and
no categories: `accept_all` on nodes, point geometry, nothing else. The output
is an id + coordinate table and nothing more.

It exists to isolate the **write path**. Because there is no classification
work to speak of, essentially the whole run is Pass B streaming rows into
Postgres, so it measures COPY throughput rather than CPU — germany-latest
sits around 350% CPU on 8 cores, i.e. mostly waiting on the database. That
makes it the benchmark where output *bytes* dominate runtime, and the one
that shows up changes the tag-heavy configs hide.

```
cargo build --release --bin osmnexus
/usr/bin/time -v ./target/release/osmnexus germany-latest.osm.pbf \
  --config-dir configs/idcoords --db-name <scratch-db> --truncate --threads 8
```

Germany is 434,024,926 node rows, so expect tens of GB in the target database
and a run measured in minutes, not seconds. Point it at a scratch database.

## Why the output switches are set the way they are

`"meta": false` and `"id_type": "none"` are the whole point rather than
incidental tuning — they are what an id+coords table would actually want, and
between them they took this import from 32:15 / 70 GB to 15:06 / 18 GB:

- **`meta`** (`updated_at`/`updated_by`/`changeset_id`) measured 87 bytes/row
  against 10 for `osm_id` + `osm_type`, and costs a `chrono` `strftime` plus a
  `String` per row to produce. For a coordinate table it is pure overhead.
- **`id_type: none`** drops the `id` column, which for a topic that never
  side-splits is exactly `"node/" + osm_id` — a restatement of the two columns
  beside it. Uniqueness moves to `(osm_type, osm_id)`.

Flipping either of these on a table that already exists needs the table
dropped first: `CREATE TABLE IF NOT EXISTS` will not add or remove a column,
and the COPY then fails against the stale schema.

## Comparing against a tag-heavy config

`configs/tilda` is the opposite shape — real classification, side splits,
producers — and is CPU-bound rather than COPY-bound. A change that helps one
often does nothing for the other, so it is worth running both before drawing
conclusions about "the pipeline getting faster".
