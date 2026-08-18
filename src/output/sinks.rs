//! Writer/channel plumbing for the pipeline's output tables — the piece that used to be ~350 lines
//! of repetitive spawn/route/await/count code sitting directly in `main.rs`. One [`TableWriters`]
//! owns every sender, round-robin counter, and writer-task handle for every table kind (tag, edges,
//! members, node geometry, way geometry, nodes); `main.rs` just calls `spawn`, routes through the
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
use crate::geom::rows::{EdgeRow, GeomRow, NodeRow, EDGE_COLUMNS, GEOM_COLUMNS, NODE_COLUMNS};
use crate::output::rows::{tag_columns, BinaryRow, MemberRow, TopicRow, MEMBER_COLUMNS};
use crate::output::writers::{binary_file_writer, copy_writer, csv_writer};
use crate::topic::spec::GeometryShape;

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

impl<T: BinaryRow + Send + Sync + 'static> Shard<T> {
    fn spawn(output: Output, pool: &Option<Pool>, out_dir: &Path, table: &str, columns: &'static str, w: usize) -> Self {
        let (mut senders, mut handles) = (Vec::with_capacity(w), Vec::with_capacity(w));
        let mut bufs = Vec::with_capacity(w);
        for _ in 0..w {
            let (tx, rx) = mpsc::channel::<Vec<T>>(WRITER_CHAN_CAP);
            let h = match output {
                Output::Pg => tokio::spawn(copy_writer::<T>(pool.clone().unwrap(), table.to_owned(), columns, rx)),
                Output::Csv => tokio::spawn(csv_writer::<T>(out_dir.join(format!("{table}.csv")), columns, rx)),
                // Staged for `output::geojson`'s post-run cursor join — the same binary wire format
                // `Output::Pg` streams live, written to a file instead (see
                // `writers::binary_file_writer`'s own doc). No header line/SQL, so `columns` (used
                // by the other two arms) doesn't apply here.
                Output::GeoJson | Output::GeoJsonSeq => {
                    tokio::spawn(binary_file_writer::<T>(out_dir.join(format!("{table}.bin")), rx))
                }
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
    /// One shard per topic in `plan.node_geom_topics` — node points only (see `GeomRow`'s own doc
    /// for why node/way geometry now live in physically separate tables: sharing one made a
    /// forward-cursor join impossible downstream, since node points are written during the select
    /// phase and way points during materialize, in unrelated relative order).
    node_geom: Vec<Shard<GeomRow>>,
    node_geom_tables: Vec<String>,
    /// One shard per topic in `plan.way_geom_topics` — whichever single shape
    /// (`plan.way_shape[i]`) that topic declared for ways.
    way_geom: Vec<Shard<GeomRow>>,
    way_geom_tables: Vec<String>,
    nodes: Shard<NodeRow>,
}

/// Counts collected once the select-phase writers (tag + members + node geometry) finish draining.
pub struct SelectCounts {
    pub tag_counts: Vec<usize>,
    pub member_count: usize,
    pub node_geom_counts: Vec<usize>,
}

/// Counts collected once the materialize-phase writers (edges/nodes/way geometry) finish draining.
pub struct MaterializeCounts {
    pub edge_count: usize,
    pub node_count: usize,
    pub way_geom_counts: Vec<usize>,
}

impl TableWriters {
    /// Spawn every writer task this run needs: `w` shards per tag table, plus shared edges/nodes
    /// (only when `plan.any_way_graph`), members, one shard per topic in `plan.node_geom_topics`
    /// (node points), and one shard per topic in `plan.way_geom_topics` (whichever single way shape
    /// each declared — see `GeometryPlan`'s own doc). `table_refs[i]` is topic `i`'s table name.
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

        let node_geom_tables: Vec<String> =
            plan.node_geom_topics.iter().map(|&i| schema::node_geom_table(table_refs[i])).collect();
        let node_geom: Vec<Shard<GeomRow>> =
            node_geom_tables.iter().map(|t| Shard::spawn(output, pool, out_dir, t, GEOM_COLUMNS, w)).collect();

        let way_geom_tables: Vec<String> =
            plan.way_geom_topics.iter().map(|&i| schema::way_geom_table(table_refs[i])).collect();
        let way_geom: Vec<Shard<GeomRow>> =
            way_geom_tables.iter().map(|t| Shard::spawn(output, pool, out_dir, t, GEOM_COLUMNS, w)).collect();

        TableWriters {
            tables: tables.to_vec(),
            tag,
            edges,
            members,
            node_geom,
            node_geom_tables,
            way_geom,
            way_geom_tables,
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

    /// Fan a node's point row out to every topic that kept this node and declared
    /// `"geometry_output": { "node": "point" }` (`plan.node_geom_topics`).
    pub fn route_node_point(&self, mask: u32, row: GeomRow, plan: &GeometryPlan) {
        for (i, &topic_idx) in plan.node_geom_topics.iter().enumerate() {
            if mask & (1 << topic_idx) != 0 {
                self.node_geom[i].send(vec![row.clone()]);
            }
        }
    }

    /// Route every shape a materialized way produced (edges, plus whichever of line/point/polygon
    /// exist) to its writer(s) — edges go to the shared graph table; line/point/polygon each go to
    /// every topic in `plan.way_geom_topics` whose declared shape (`plan.way_shape[i]`) matches
    /// which one of `g`'s fields is `Some`.
    pub fn route_way(&self, mask: u32, g: WayGeometry, plan: &GeometryPlan) {
        if let Some(rows) = g.edges {
            self.edges.send(rows);
        }
        for (i, &topic_idx) in plan.way_geom_topics.iter().enumerate() {
            if mask & (1 << topic_idx) == 0 {
                continue;
            }
            let row = match plan.way_shape[i] {
                GeometryShape::Line => &g.line,
                GeometryShape::Point => &g.point,
                GeometryShape::Polygon => &g.polygon,
            };
            if let Some(row) = row {
                self.way_geom[i].send(vec![row.clone()]);
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

    /// Drain + await the select-phase writers (tag + members + node geometry) and log their counts.
    /// Node geometry is fully select-phase now (way geometry lives in its own separate table — see
    /// `GeomRow`'s own doc), so unlike the old shared `point` table, it's finished here rather than
    /// carried into the materialize phase.
    pub async fn finish_select(self) -> anyhow::Result<(Self, SelectCounts)> {
        // `Shard::finish` consumes; split tag/members/node_geom out, finish them, and reassemble
        // the rest so the caller keeps one `TableWriters` for the materialize phase.
        let TableWriters { tables, tag, edges, members, node_geom, node_geom_tables, way_geom, way_geom_tables, nodes } = self;
        let mut tag_counts = Vec::with_capacity(tag.len());
        for shard in tag {
            tag_counts.push(shard.finish().await?);
        }
        let member_count = members.finish().await?;
        let mut node_geom_counts = Vec::with_capacity(node_geom.len());
        for shard in node_geom {
            node_geom_counts.push(shard.finish().await?);
        }

        for (table, &count) in tables.iter().zip(&tag_counts) {
            info!("Wrote {count} tag rows → {table}");
        }
        info!("Wrote {member_count} relation-member links → {MEMBER_TABLE}");
        for (table, &count) in node_geom_tables.iter().zip(&node_geom_counts) {
            info!("Wrote {count} rows → {table}");
        }

        let rest = TableWriters {
            tables,
            tag: Vec::new(),
            edges,
            members: Shard::empty(),
            node_geom: Vec::new(),
            node_geom_tables: Vec::new(),
            way_geom,
            way_geom_tables,
            nodes,
        };
        Ok((rest, SelectCounts { tag_counts, member_count, node_geom_counts }))
    }

    /// Drain + await the materialize-phase writers (edges/nodes/way geometry) and log their counts.
    pub async fn finish_materialize(self, any_way_graph: bool) -> anyhow::Result<MaterializeCounts> {
        let edge_count = self.edges.finish().await?;
        let node_count = self.nodes.finish().await?;
        let mut way_geom_counts = Vec::with_capacity(self.way_geom.len());
        for shard in self.way_geom {
            way_geom_counts.push(shard.finish().await?);
        }

        if any_way_graph {
            info!("Wrote {edge_count} edge rows → {EDGE_TABLE}");
            info!("Wrote {node_count} node rows → {NODE_TABLE}");
        }
        for (table, &count) in self.way_geom_tables.iter().zip(&way_geom_counts) {
            info!("Wrote {count} rows → {table}");
        }

        Ok(MaterializeCounts { edge_count, node_count, way_geom_counts })
    }
}

/// Write an already-fully-built (small) batch of rows to `table` in one shot — used for relation
/// geometry, which is resolved entirely in memory *after* the main streaming pass (see
/// `geom::relation`), so it has no ongoing channel to shard across `w` writers like the streaming
/// tables above; one connection/file is plenty for what's typically a small dataset.
pub async fn write_rows_once<R: BinaryRow + Send + Sync + 'static>(
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
        Output::Csv => tokio::spawn(csv_writer::<R>(out_dir.join(format!("{table}.csv")), columns, rx)),
        Output::GeoJson | Output::GeoJsonSeq => {
            tokio::spawn(binary_file_writer::<R>(out_dir.join(format!("{table}.bin")), rx))
        }
    };
    tx.send(rows).await.ok();
    drop(tx);
    handle.await.context("relation-geometry writer panicked")?
}
