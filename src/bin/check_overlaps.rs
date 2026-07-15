use anyhow::Result;

use osmnexus::categorize::linter::find_all_topic_overlaps;

/// Lint: report category pairs that can match the same object without excluding each other
/// (first-match order would then silently pick the winner). Runs across every topic. The same
/// check is enforced by the `categories_are_disjoint` test; this binary is for ad-hoc inspection.
fn main() -> Result<()> {
    let mut found_any = false;
    for (topic, overlaps) in find_all_topic_overlaps()? {
        println!("── {topic} ──");
        if overlaps.is_empty() {
            println!("  No overlaps found.");
            continue;
        }
        found_any = true;
        for o in overlaps {
            println!("  {}  <->  {}", o.a, o.b);
            for w in &o.warnings {
                println!("    Warning: {w}");
            }
        }
    }
    if found_any {
        std::process::exit(1);
    }
    Ok(())
}
