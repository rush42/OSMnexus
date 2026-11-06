//! Emit a topic's output fields as TypeScript type declarations — one `export type` per field.
//! The type of each field comes from the topic's inferred `OutputSchema` (`topic::schema`): a
//! closed value set becomes a string-literal union, anything else a plain `string`. This binary
//! only reads the config and prints; inference lives in `osmnexus-lang`, rendering in
//! `osmnexus-codegen` (`codegen::ts`).
//!
//! Usage: `export_ts_types <config-dir> <topic-name>`, e.g. `export_ts_types configs tilda/bikelanes`.
//! Prints the `.ts` source to stdout.

use anyhow::{Context, Result};


fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: export_ts_types <config-dir> <topic-name>";
    let config_dir = args.next().context(usage)?;
    let topic_name = args.next().context(usage)?;

    let config = osmnexus::config_dir::Config::read(&config_dir, false)?;
    // `0` skips compiling any decision tree — this binary only inspects the producers' field
    // shapes, never a compiled tree.
    let topic = config.load(&topic_name, 0)
        .with_context(|| format!("loading topic '{topic_name}'"))?;

    print!("{}", osmnexus::codegen::ts::render_ts(&topic_name, &topic.output_schema()));
    Ok(())
}
