use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::lang::env::CompileEnv;
use crate::topic::sources::{join, ConfigSources};

use crate::topic::load::{
    load_shared_macros, load_topic_categories, load_topic_macros, load_topic_sanitizers, resolve_macros,
};
use crate::categorize::categories::CategoriesFile;
use crate::lang::filter::Filter;
use crate::lang::expr::{filter_to_expr, to_dnf, to_nnf, Literal, Predicate};
use crate::lang::tags::ElementKind;

// ── Overlap detection ───────────────────────────────────────────────────────────
//
// Two categories "overlap" when their conditions can be satisfied by the same object *and*
// neither excludes the other — i.e. a way could match both, so first-match order silently
// decides the winner. This is a conservative heuristic (numeric/`sanitize` atoms are treated as
// independent literals, not reasoned about), so it may report a false overlap but never misses a
// genuine structural one.

/// A detected overlap between two non-excluding categories, with any divergence warnings.
#[derive(Debug, Clone)]
pub struct Overlap {
    pub a: String,
    pub b: String,
    pub warnings: Vec<String>,
}

/// Returns (is_consistent, warnings) for a single DNF term (conjunction of literals).
fn check_term_consistency(term: &[Literal]) -> (bool, Vec<String>) {
    let mut warnings = Vec::new();

    // 1. Exact contradiction: A & Not(A)
    for lit in term {
        if let Literal::Pos(p) = lit {
            if term.contains(&Literal::Neg(p.clone())) {
                return (false, vec![]); // strict contradiction
            }
        }
    }

    // 2. Group by involved tag for domain-specific checks
    let mut by_tag: HashMap<String, Vec<&Literal>> = HashMap::new();
    for lit in term {
        let p = match lit {
            Literal::Pos(p) => p,
            Literal::Neg(p) => p,
        };
        for t in p.tags_involved() {
            by_tag.entry(t).or_default().push(lit);
        }
    }

    for (tag, lits) in by_tag {
        let mut eqs = HashSet::new();
        let mut not_eqs = HashSet::new();
        let mut starts_with = Vec::new();
        let mut contains = Vec::new();
        let mut exact_not_exists = false;

        for lit in &lits {
            match lit {
                Literal::Pos(Predicate::Eq(_, v)) => { eqs.insert(v); },
                Literal::Neg(Predicate::Eq(_, v)) => { not_eqs.insert(v); },
                Literal::Pos(Predicate::StartsWith(_, v)) => { starts_with.push(v); },
                Literal::Pos(Predicate::Contains(_, v)) => { contains.push(v); },
                Literal::Neg(Predicate::Exists(_)) => { exact_not_exists = true; },
                _ => {}
            }
        }

        if eqs.len() > 1 {
            return (false, vec![]); // Eq("a") AND Eq("b") for the same tag
        }

        if !eqs.is_empty() && exact_not_exists {
            return (false, vec![]); // Eq(_) AND Not(Exists)
        }

        if let Some(&eq_val) = eqs.iter().next() {
            if not_eqs.contains(eq_val) {
                return (false, vec![]); // Eq("a") AND Not(Eq("a"))
            }
            for &sw in &starts_with {
                if !eq_val.starts_with(sw) {
                    warnings.push(format!("Tag '{}' Eq({:?}) diverges from StartsWith({:?})", tag, eq_val, sw));
                }
            }
            for &c in &contains {
                if !eq_val.contains(c) {
                    warnings.push(format!("Tag '{}' Eq({:?}) diverges from Contains({:?})", tag, eq_val, c));
                }
            }
        }
    }

    (true, warnings)
}

/// Find all overlapping category pairs within one loaded topic's categories.
pub fn find_overlaps(cats: &CategoriesFile) -> Vec<Overlap> {
    // Precompute each category's DNF, keeping only internally consistent terms.
    let mut category_dnfs: HashMap<String, Vec<Vec<Literal>>> = HashMap::new();
    for cat in &cats.categories {
        let dnf = to_dnf(to_nnf(filter_to_expr(&cat.condition)));
        let consistent: Vec<_> = dnf.into_iter().filter(|t| check_term_consistency(t).0).collect();
        category_dnfs.insert(cat.id.clone(), consistent);
    }

    let names: Vec<&str> = cats.categories.iter().map(|c| c.id.as_str()).collect();
    let mut overlaps = Vec::new();

    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let (a, b) = (names[i], names[j]);

            // Explicit mutual exclusion is handled by first-match logic natively — skip.
            let a_def = &cats.categories[i];
            let b_def = &cats.categories[j];
            let a_excl_b = a_def.excludes.as_ref().is_some_and(|ex| ex.iter().any(|e| e == b));
            let b_excl_a = b_def.excludes.as_ref().is_some_and(|ex| ex.iter().any(|e| e == a));
            if a_excl_b || b_excl_a {
                continue;
            }

            let mut overlaps_here = false;
            let mut warnings = Vec::new();
            for t_a in &category_dnfs[a] {
                for t_b in &category_dnfs[b] {
                    let mut combined = t_a.clone();
                    combined.extend(t_b.clone());
                    let (consistent, w) = check_term_consistency(&combined);
                    if consistent {
                        overlaps_here = true;
                        warnings.extend(w);
                    }
                }
            }

            if overlaps_here {
                warnings.sort();
                warnings.dedup();
                overlaps.push(Overlap { a: a.to_owned(), b: b.to_owned(), warnings });
            }
        }
    }

    overlaps
}

/// Every topic directory holding at least one category kind subfolder, by name, sorted (skips
/// shared config-root files and any topic without categories). Used by lint + tests.
pub fn topic_category_dirs(src: &ConfigSources) -> Vec<String> {
    src.subdirs("")
        .into_iter()
        .filter(|topic| ElementKind::ALL.iter().any(|k| src.is_dir(&join(topic, k.subdir()))))
        .collect()
}

/// Load every topic's per-kind categories, fully macro/sanitizer-resolved (the same
/// `topic::load` pipeline `Topic::load` uses, minus the `producers`/`transforms.json`/
/// `producers.json` machinery this lint has no use for), and collect overlaps per (topic, kind).
/// Shared entry point.
pub fn find_all_topic_overlaps(src: &ConfigSources, env: &Arc<CompileEnv>) -> anyhow::Result<Vec<(String, Vec<Overlap>)>> {
    crate::lang::env::with_env(env, || find_all_topic_overlaps_in_env(src))
}

fn find_all_topic_overlaps_in_env(src: &ConfigSources) -> anyhow::Result<Vec<(String, Vec<Overlap>)>> {
    let mut out = Vec::new();
    for topic in topic_category_dirs(src) {
        let sanitizers = load_topic_sanitizers(src, &topic)
            .map_err(|e| anyhow::anyhow!("loading {topic} sanitizers: {e}"))?;
        let shared_macros = load_shared_macros(src)
            .map_err(|e| anyhow::anyhow!("loading shared macros.json: {e}"))?;
        let topic_macros = load_topic_macros(src, &topic)
            .map_err(|e| anyhow::anyhow!("loading {topic} macros: {e}"))?;
        let raw_macros = crate::topic::load::merge(&shared_macros, &topic_macros);
        let resolved_macros = resolve_macros(&raw_macros)
            .map_err(|e| anyhow::anyhow!("resolving {topic} macros: {e}"))?;
        let macros: HashMap<String, Filter> = resolved_macros.iter()
            .map(|(k, v)| Ok((k.clone(), serde_json::from_value(crate::topic::load::inline_sanitize_refs(v.clone(), &sanitizers)?)?)))
            .collect::<anyhow::Result<_>>()
            .map_err(|e: anyhow::Error| anyhow::anyhow!("parsing {topic} resolved macros: {e}"))?;

        let per_kind = load_topic_categories(src, &topic, &resolved_macros, &macros, &sanitizers)
            .map_err(|e| anyhow::anyhow!("loading {topic} categories: {e}"))?;
        for (kind, cats) in per_kind {
            let label = format!("{topic}/{}", kind.subdir());
            out.push((label, find_overlaps(&cats)));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::find_all_topic_overlaps;

    /// Fails if any two categories in a topic can match the same object without excluding each
    /// other — first-match order would then silently decide the winner. Conservative: may flag a
    /// false positive (add an `excludes` entry to resolve), never misses a structural overlap.
    #[test]
    fn categories_are_disjoint() {
        let cfg = crate::test_support::Config::default_root().expect("reading config");
        let per_topic = find_all_topic_overlaps(&cfg.sources, &cfg.env).expect("loading topic categories");
        let mut msg = String::new();
        for (topic, overlaps) in &per_topic {
            for o in overlaps {
                msg.push_str(&format!("\n  [{topic}] {} <-> {}", o.a, o.b));
                for w in &o.warnings {
                    msg.push_str(&format!("\n      warning: {w}"));
                }
            }
        }
        assert!(msg.is_empty(), "overlapping categories found:{msg}\n");
    }
}
