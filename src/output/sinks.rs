//! Writer/channel plumbing for the pipeline's output tables — the piece that used to be ~350 lines
//! of repetitive spawn/route/await/count code sitting directly in `main.rs`. One [`TableWriters`]
//! owns every sender, round-robin counter, and writer-task handle for every table kind (tag, edges,
//! members, way-line, polygon, point, nodes); `main.rs` just calls `spawn`, routes through the
//! `route_*` methods from inside its select/materialize closures, and calls `finish_select`/
//! `finish_materialize` to drain + collect counts. No policy here — same sharded-COPY/CSV writer
//! tasks (`output::writers::{copy_writer, csv_writer}`) and round-robin fan-out as before, just
//! owned by one value instead of a dozen loose `main.rs` locals.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Context;
use deadpool_postgres::Pool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::info;

use crate::config::Output;
use crate::db::schema::{self, EDGE_TABLE, MEMBER_TABLE, NODE_TABLE};
use crate::geom::plan::GeometryPlan;
use crate::geom::materialize::WayGeometry;
use crate::geom::rows::{
    EdgeRow, NodeRow, PointRow, PolygonRow, WayRow, EDGE_COLUMNS, NODE_COLUMNS, POINT_COLUMNS,
    POLYGON_COLUMNS, WAY_COLUMNS,
};
use crate::output::rows::{tag_columns, BinaryRow, CsvRow, MemberRow, TopicRow, MEMBER_COLUMNS};
use crate::output::writers::{copy_writer, csv_writer};

/// Per-writer channel capacity (rows/batches buffered before the producer blocks).
const WRITER_CHAN_CAP: usize = 256;

/// Single-row `send` calls (e.g. one point row per node/way) accumulate here per shard instead of
/// going straight to the channel — a channel send costs a permit acquire + wake-up + allocation
/// per call, which dominates when callers hand rows in one at a time (`route_point`'s per-node
/// `vec![row.clone()]`, previously the single biggest contributor to per-node overhead on an
/// all-nodes import). Batches flush once a shard's buffer reaches this size, and whatever's left
/// flushes in `finish`. `route_tag`'s already-batched sends (a full blob's worth of rows) skip
/// this buffer entirely — see `send`'s own doc.
const SEND_BATCH: usize = 2048;

/// `w` sharded writers for one table, sender + round-robin counter + join handle per shard, plus
/// one small-row accumulation buffer per shard (see `SEND_BATCH`'s own doc).
struct Shard<T> {
    senders: Vec<mpsc::Sender<Vec<T>>>,
    rr: AtomicUsize,
    handles: Vec<JoinHandle<anyhow::Result<usize>>>,
    bufs: Vec<Mutex<Vec<T>>>,
}

impl<T: CsvRow + BinaryRow + Send + Sync + 'static> Shard<T> {
    fn spawn(output: Output, pool: &Option<Pool>, out_dir: &Path, table: &str, columns: &'static str, w: usize) -> Self {
        let (mut senders, mut handles) = (Vec::with_capacity(w), Vec::with_capacity(w));
        let mut bufs = Vec::with_capacity(w);
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<T>>(WRITER_CHAN_CAP);
            let h = match output {
                Output::Pg => tokio::spawn(copy_writer::<T>(pool.clone().unwrap(), table.to_owned(), columns, rx)),
                Output::Csv | Output::GeoJson | Output::GeoJsonSeq | Output::Parquet => tokio::spawn(csv_writer::<T>(out_dir.join(format!("{table}.csv")), columns, rx)),
            };
            handles.push(h);
            senders.push(tx);
            bufs.push(Mutex::new(Vec::with_capacity(SEND_BATCH)));
        }
        Shard { senders, rr: AtomicUsize::new(0), handles, bufs }
    }

    fn empty() -> Self {
        Shard { senders: Vec::new(), rr: AtomicUsize::new(0), handles: Vec::new(), bufs: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }

    /// Route `rows` to one shard, round-robin. Already-batch-sized sends (`route_tag`'s full
    /// blob-fold `Vec`s) go straight to the channel unchanged — they're exactly the case the
    /// channel was designed for. Small sends (typically length 1, from `route_point`/`route_shape`
    /// handing rows in one at a time) accumulate in that shard's buffer instead, and only cross
    /// the channel once `SEND_BATCH` rows are pending, cutting the per-row channel-send count by
    /// ~`SEND_BATCH`x for that path.
    fn send(&self, rows: Vec<T>) {
        if self.senders.is_empty() {
            return;
        }
        let w = self.senders.len();
        let kk = self.rr.fetch_add(1, Ordering::Relaxed) % w;
        if rows.len() >= SEND_BATCH {
            let _ = self.senders[kk].blocking_send(rows);
            return;
        }
        let mut buf = self.bufs[kk].lock().unwrap();
        buf.extend(rows);
        if buf.len() >= SEND_BATCH {
            let batch = std::mem::replace(&mut *buf, Vec::with_capacity(SEND_BATCH));
            drop(buf);
            let _ = self.senders[kk].blocking_send(batch);
        }
    }

    async fn finish(self) -> anyhow::Result<usize> {
        // `.await`, not `blocking_send` — unlike `send` (called from sync rayon-worker callbacks),
        // `finish` runs in async context, and `blocking_send` panics if called from one.
        for (tx, buf) in self.senders.iter().zip(&self.bufs) {
            let batch = std::mem::take(&mut *buf.lock().unwrap());
            if !batch.is_empty() {
                let _ = tx.send(batch).await;
            }
        }
        drop(self.senders);
        let mut count = 0;
        for h in self.handles {
            count += h.await.context("writer panicked")??;
        }
        Ok(count)
    }
}

/// Every writer-table's senders/handles for one run, spawned once up front and consumed by the
/// select and materialize phases in turn — see this module's own doc.
pub struct TableWriters {
    tables: Vec<String>,
    tag: Vec<Shard<TopicRow>>,
    edges: Shard<EdgeRow>,
    members: Shard<MemberRow>,
    way_line: Vec<Shard<WayRow>>,
    way_line_tables: Vec<String>,
    polygon: Vec<Shard<PolygonRow>>,
    polygon_tables: Vec<String>,
    point: Vec<Shard<PointRow>>,
    point_tables: Vec<String>,
    nodes: Shard<NodeRow>,
}

/// Counts collected once the select-phase writers (tag + members) finish draining.
pub struct SelectCounts {
    pub tag_counts: Vec<usize>,
    pub member_count: usize,
}

/// Counts collected once the materialize-phase writers (edges/nodes/way-line/polygon/point) finish
/// draining. `point_counts` covers node-point rows (sent during select) and way-point rows (sent
/// during materialize) alike — the two phases share the same point channels.
pub struct MaterializeCounts {
    pub edge_count: usize,
    pub node_count: usize,
    pub way_line_counts: Vec<usize>,
    pub polygon_counts: Vec<usize>,
    pub point_counts: Vec<usize>,
}

impl TableWriters {
    /// Spawn every writer task this run needs: `w` shards per tag table, plus shared
    /// edges/nodes (only when `plan.any_way_graph`), members, and one shard-set per topic wanting a
    /// way-line/polygon/point table (`geom_tables`/`plan` already say which topics those are —
    /// see `GeometryPlan`'s own doc). `table_refs[i]` is topic `i`'s table name.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        output: Output,
        pool: &Option<Pool>,
        out_dir: &Path,
        w: usize,
        tables: &[String],
        table_refs: &[&str],
        emits_id: &[bool],
        plan: &GeometryPlan,
    ) -> Self {
        // Column list per table, not one shared constant: a topic that set `"id_type": "none"`
        // drops the `id` column, and the COPY statement/CSV header must match the rows it receives.
        let tag: Vec<Shard<TopicRow>> = tables
            .iter()
            .zip(emits_id)
            .map(|(t, &emits)| Shard::spawn(output, pool, out_dir, t, tag_columns(emits), w))
            .collect();

        let edges = if plan.any_way_graph {
            Shard::spawn(output, pool, out_dir, EDGE_TABLE, EDGE_COLUMNS, w)
        } else {
            Shard::empty()
        };
        let nodes = if plan.any_way_graph {
            Shard::spawn(output, pool, out_dir, NODE_TABLE, NODE_COLUMNS, w)
        } else {
            Shard::empty()
        };
        let members = Shard::spawn(output, pool, out_dir, MEMBER_TABLE, MEMBER_COLUMNS, w);

        let way_line_tables: Vec<String> = plan.way_line_topics.iter().map(|&i| schema::way_geom_table(table_refs[i])).collect();
        let way_line: Vec<Shard<WayRow>> =
            way_line_tables.iter().map(|t| Shard::spawn(output, pool, out_dir, t, WAY_COLUMNS, w)).collect();

        let polygon_tables: Vec<String> = plan.way_polygon_topics.iter().map(|&i| schema::polygon_table(table_refs[i])).collect();
        let polygon: Vec<Shard<PolygonRow>> =
            polygon_tables.iter().map(|t| Shard::spawn(output, pool, out_dir, t, POLYGON_COLUMNS, w)).collect();

        let point_tables: Vec<String> = plan.point_topics.iter().map(|&i| schema::point_table(table_refs[i])).collect();
        let point: Vec<Shard<PointRow>> =
            point_tables.iter().map(|t| Shard::spawn(output, pool, out_dir, t, POINT_COLUMNS, w)).collect();

        TableWriters {
            tables: tables.to_vec(),
            tag,
            edges,
            members,
            way_line,
            way_line_tables,
            polygon,
            polygon_tables,
            point,
            point_tables,
            nodes,
        }
    }

    /// Round-robin a batch of per-topic tag rows out to each topic's tag-table shard. Returns
    /// whether any topic produced rows (i.e. the element was "kept" by some topic).
    pub fn route_tag(&self, rows: Vec<Vec<TopicRow>>) -> bool {
        let mut any = false;
        for (i, r) in rows.into_iter().enumerate() {
            if !r.is_empty() {
                any = true;
                self.tag[i].send(r);
            }
        }
        any
    }

    pub fn route_member(&self, links: Vec<MemberRow>) {
        self.members.send(links);
    }

    /// Fan a `point` row out to every topic that both declared `point` (on whichever kind is
    /// calling — `eligible` gates that) and kept this element (`mask`).
    pub fn route_node_point(&self, mask: u32, row: PointRow, plan: &GeometryPlan) {
        self.route_point(mask, row, &plan.point_topics, &plan.node_point_eligible);
    }

    fn route_point(&self, mask: u32, row: PointRow, point_topics: &[usize], eligible: &[bool]) {
        for (i, &topic_idx) in point_topics.iter().enumerate() {
            if eligible[i] && mask & (1 << topic_idx) != 0 {
                self.point[i].send(vec![row.clone()]);
            }
        }
    }

    /// Route every shape a materialized way produced (edges/line/point/polygon) to its writer(s) —
    /// one call per way, replacing four separate `if let Some(...)` blocks at the call site.
    pub fn route_way(&self, mask: u32, g: WayGeometry, plan: &GeometryPlan) {
        if let Some(rows) = g.edges {
            self.edges.send(rows);
        }
        if let Some(row) = g.line {
            self.route_shape(&self.way_line, mask, &plan.way_line_topics, row);
        }
        if let Some(row) = g.point {
            self.route_point(mask, row, &plan.point_topics, &plan.way_point_eligible);
        }
        if let Some(row) = g.polygon {
            self.route_shape(&self.polygon, mask, &plan.way_polygon_topics, row);
        }
    }

    fn route_shape<T: CsvRow + BinaryRow + Send + Sync + Clone + 'static>(&self, shards: &[Shard<T>], mask: u32, topics: &[usize], row: T) {
        for (i, &topic_idx) in topics.iter().enumerate() {
            if mask & (1 << topic_idx) != 0 {
                shards[i].send(vec![row.clone()]);
            }
        }
    }

    pub fn route_node_rows(&self, mut rows: Vec<NodeRow>) {
        if self.nodes.is_empty() {
            return;
        }
        while !rows.is_empty() {
            let take = rows.len().min(4096);
            let chunk: Vec<NodeRow> = rows.drain(..take).collect();
            self.nodes.send(chunk);
        }
    }

    /// Drain + await the select-phase writers (tag + members) and log their counts. Point-table
    /// writers are *not* touched here — they stay open for the materialize phase's way-point rows.
    pub async fn finish_select(self) -> anyhow::Result<(Self, SelectCounts)> {
        // `Shard::finish` consumes; split tag/members out, finish them, and reassemble the rest so
        // the caller keeps one `TableWriters` for the materialize phase.
        let TableWriters { tables, tag, edges, members, way_line, way_line_tables, polygon, polygon_tables, point, point_tables, nodes } = self;
        let mut tag_counts = Vec::with_capacity(tag.len());
        for shard in tag {
            tag_counts.push(shard.finish().await?);
        }
        let member_count = members.finish().await?;

        for (table, &count) in tables.iter().zip(&tag_counts) {
            info!("Wrote {count} tag rows → {table}");
        }
        info!("Wrote {member_count} relation-member links → {MEMBER_TABLE}");

        let rest = TableWriters {
            tables,
            tag: Vec::new(),
            edges,
            members: Shard::empty(),
            way_line,
            way_line_tables,
            polygon,
            polygon_tables,
            point,
            point_tables,
            nodes,
        };
        Ok((rest, SelectCounts { tag_counts, member_count }))
    }

    /// Drain + await the materialize-phase writers (edges/nodes/way-line/polygon/point) and log
    /// their counts.
    pub async fn finish_materialize(self, any_way_graph: bool) -> anyhow::Result<MaterializeCounts> {
        let edge_count = self.edges.finish().await?;
        let mut way_line_counts = Vec::with_capacity(self.way_line.len());
        for shard in self.way_line {
            way_line_counts.push(shard.finish().await?);
        }
        let node_count = self.nodes.finish().await?;
        let mut polygon_counts = Vec::with_capacity(self.polygon.len());
        for shard in self.polygon {
            polygon_counts.push(shard.finish().await?);
        }
        let mut point_counts = Vec::with_capacity(self.point.len());
        for shard in self.point {
            point_counts.push(shard.finish().await?);
        }

        if any_way_graph {
            info!("Wrote {edge_count} edge rows → {EDGE_TABLE}");
            info!("Wrote {node_count} node rows → {NODE_TABLE}");
        }
        for (table, &count) in self.way_line_tables.iter().zip(&way_line_counts) {
            info!("Wrote {count} way rows → {table}");
        }
        for (table, &count) in self.polygon_tables.iter().zip(&polygon_counts) {
            info!("Wrote {count} rows → {table}");
        }
        for (table, &count) in self.point_tables.iter().zip(&point_counts) {
            info!("Wrote {count} rows → {table}");
        }

        Ok(MaterializeCounts { edge_count, node_count, way_line_counts, polygon_counts, point_counts })
    }
}

/// Write an already-fully-built (small) batch of rows to `table` in one shot — used for relation
/// geometry, which is resolved entirely in memory *after* the main streaming pass (see
/// `geom::relation`), so it has no ongoing channel to shard across `w` writers like the streaming
/// tables above; one connection/file is plenty for what's typically a small dataset.
pub async fn write_rows_once<R: CsvRow + BinaryRow + Send + Sync + 'static>(
    output: Output,
    pool: &Option<Pool>,
    out_dir: &Path,
    table: &str,
    columns: &'static str,
    rows: Vec<R>,
) -> anyhow::Result<usize> {
    let (tx, rx) = mpsc::channel(1);
    let handle = match output {
        Output::Pg => tokio::spawn(copy_writer::<R>(pool.clone().unwrap(), table.to_owned(), columns, rx)),
        Output::Csv | Output::GeoJson | Output::GeoJsonSeq | Output::Parquet => {
            tokio::spawn(csv_writer::<R>(out_dir.join(format!("{table}.csv")), columns, rx))
        }
    };
    tx.send(rows).await.ok();
    drop(tx);
    handle.await.context("relation-geometry writer panicked")?
}
