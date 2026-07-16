use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::topic::load::{
    load_shared_macros, load_topic_categories, load_topic_macros, load_topic_sanitizers, resolve_macros,
};
use crate::categorize::categories::CategoriesFile;
use crate::lang::extract::Extract;
use crate::lang::filter::Filter;
use crate::osm::types::ElementKind;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Predicate {
    Eq(String, String),
    Contains(String, String),
    StartsWith(String, String),
    EndsWith(String, String),
    Exists(String),
    FirstTagIn(Vec<String>, Vec<String>),
    /// Numeric comparison atom: `(value key, op, threshold bits)`. The threshold is stored as the
    /// f64 bit pattern so the atom stays `Hash`/`Eq`/`Ord`; identity is exact-literal, which is all
    /// the overlap lint needs (it won't *prove* e.g. `lte 0.08 ⟹ lte 0.13`, only treats atoms as
    /// independent — sound but conservative).
    Num(String, NumOp, u64),
    HasKeyPrefix(String),
    HasParent,
    Prefix(String),
    Infix(String),
    Side(String),
    /// `Filter::TagsEmpty` — whether the object's own tags are (non-)empty. Practically never
    /// appears in a real category condition (it's `InputTransform::Drop`'s own mechanism), but
    /// `Filter` is one type shared by both, so `filter_to_expr` needs a total mapping regardless.
    TagsEmpty,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NumOp { Lt, Lte, Gt, Gte }

impl Predicate {
    pub fn tags_involved(&self) -> Vec<String> {
        match self {
            Predicate::Eq(k, _) => vec![k.clone()],
            Predicate::Contains(k, _) => vec![k.clone()],
            Predicate::StartsWith(k, _) => vec![k.clone()],
            Predicate::EndsWith(k, _) => vec![k.clone()],
            Predicate::Exists(k) => vec![k.clone()],
            Predicate::FirstTagIn(ks, _) => ks.clone(),
            Predicate::Num(k, _, _) => vec![k.clone()],
            Predicate::HasKeyPrefix(p) => vec![format!("prefix({})", p)],
            Predicate::HasParent => vec!["[parent]".to_string()],
            Predicate::Prefix(_) => vec!["[prefix]".to_string()],
            Predicate::Infix(_) => vec!["[infix]".to_string()],
            Predicate::Side(_) => vec!["[side]".to_string()],
            Predicate::TagsEmpty => vec!["[tags_empty]".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Literal {
    Pos(Predicate),
    Neg(Predicate),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    True,
    False,
    Lit(Literal),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// `extract`'s key if `Value`, else the first candidate of `Candidates` — the representative key
/// used for `Predicate` atoms that can only name one plain key (see `filter_to_expr`'s doc on why
/// that's an approximation for the `Candidates`/`first_tag` case).
fn extract_key(extract: &crate::lang::extract::Extract) -> String {
    use crate::lang::extract::Extract;
    match extract {
        Extract::Value { key, .. } => key.clone(),
        Extract::Candidates { keys, .. } => keys.first().cloned()
            .expect("Extract::Candidates needs a non-empty `keys`/`first_tag`"),
    }
}

/// Convert a resolved `Filter` (no macro/named-sanitizer reference left — see `Filter`'s own doc)
/// to an overlap-analysis `Expr`. Used by both `decision_tree::build` (an already-resolved
/// `CategoriesFile`, built earlier in `topic::runner::TopicRunner::load` for unrelated reasons) and
/// the standalone overlap lint (`find_all_topic_overlaps`, which builds its own resolved
/// `CategoriesFile` via the same `topic::load` pipeline `TopicRunner` uses). `sanitize` is ignored
/// either way: the overlap lint is a conservative heuristic and treats a sanitized comparison like
/// a raw one (atoms are independent, which keeps it sound enough).
pub fn filter_to_expr(filter: &Filter) -> Expr {
    match filter {
        Filter::Bool(true) => Expr::True,
        Filter::Bool(false) => Expr::False,
        Filter::And { and } => Expr::And(and.iter().map(filter_to_expr).collect()),
        Filter::Or { or } => Expr::Or(or.iter().map(filter_to_expr).collect()),
        Filter::Not { not } => Expr::Not(Box::new(filter_to_expr(not))),

        Filter::Eq { extract, eq, .. } => match extract {
            Extract::Value { key, .. } => Expr::Lit(Literal::Pos(Predicate::Eq(key.clone(), eq.clone()))),
            Extract::Candidates { keys, .. } => Expr::Lit(Literal::Pos(Predicate::FirstTagIn(keys.clone(), vec![eq.clone()]))),
        },
        Filter::Exists { extract, exists: true, .. } => Expr::Lit(Literal::Pos(Predicate::Exists(extract_key(extract)))),
        Filter::Exists { extract, exists: false, .. } => Expr::Lit(Literal::Neg(Predicate::Exists(extract_key(extract)))),
        Filter::Contains { extract, contains, .. } => Expr::Lit(Literal::Pos(Predicate::Contains(extract_key(extract), contains.clone()))),
        Filter::StartsWith { extract, starts_with, .. } => Expr::Lit(Literal::Pos(Predicate::StartsWith(extract_key(extract), starts_with.clone()))),
        Filter::EndsWith { extract, ends_with, .. } => Expr::Lit(Literal::Pos(Predicate::EndsWith(extract_key(extract), ends_with.clone()))),
        Filter::In { extract, r#in, .. } => match extract {
            Extract::Value { key, .. } => {
                let exprs: Vec<_> = r#in.iter().map(|v| Expr::Lit(Literal::Pos(Predicate::Eq(key.clone(), v.clone())))).collect();
                Expr::Or(exprs)
            }
            Extract::Candidates { keys, .. } => Expr::Lit(Literal::Pos(Predicate::FirstTagIn(keys.clone(), r#in.clone()))),
        },
        Filter::InSet { extract, in_set, .. } => match extract {
            Extract::Value { key, .. } => {
                let exprs: Vec<_> = crate::value_sets::value_set(in_set)
                    .iter()
                    .map(|v| Expr::Lit(Literal::Pos(Predicate::Eq(key.clone(), v.clone()))))
                    .collect();
                Expr::Or(exprs)
            }
            Extract::Candidates { keys, .. } => {
                let mut vals: Vec<String> = crate::value_sets::value_set(in_set).iter().cloned().collect();
                vals.sort();
                Expr::Lit(Literal::Pos(Predicate::FirstTagIn(keys.clone(), vals)))
            }
        },

        Filter::Parent { parent } => prefix_expr_tags(filter_to_expr(parent)),

        Filter::AnnotationEq { key, eq } => match key.as_str() {
            "_side" => Expr::Lit(Literal::Pos(Predicate::Side(eq.clone()))),
            "_prefix" => Expr::Lit(Literal::Pos(Predicate::Prefix(eq.clone()))),
            "_infix" => Expr::Lit(Literal::Pos(Predicate::Infix(eq.clone()))),
            other => unreachable!("Filter::AnnotationEq only ever spelled for _side/_prefix/_infix, got {other}"),
        },
        Filter::NumLt  { extract, lt,  .. } => Expr::Lit(Literal::Pos(Predicate::Num(extract_key(extract), NumOp::Lt,  lt.to_bits()))),
        Filter::NumLte { extract, lte, .. } => Expr::Lit(Literal::Pos(Predicate::Num(extract_key(extract), NumOp::Lte, lte.to_bits()))),
        Filter::NumGt  { extract, gt,  .. } => Expr::Lit(Literal::Pos(Predicate::Num(extract_key(extract), NumOp::Gt,  gt.to_bits()))),
        Filter::NumGte { extract, gte, .. } => Expr::Lit(Literal::Pos(Predicate::Num(extract_key(extract), NumOp::Gte, gte.to_bits()))),
        Filter::HasKeyPrefix { has_key_prefix } => Expr::Lit(Literal::Pos(Predicate::HasKeyPrefix(has_key_prefix.clone()))),
        Filter::HasParent { has_parent: true } => Expr::Lit(Literal::Pos(Predicate::HasParent)),
        Filter::HasParent { has_parent: false } => Expr::Lit(Literal::Neg(Predicate::HasParent)),
        Filter::TagsEmpty { tags_empty: true } => Expr::Lit(Literal::Pos(Predicate::TagsEmpty)),
        Filter::TagsEmpty { tags_empty: false } => Expr::Lit(Literal::Neg(Predicate::TagsEmpty)),
    }
}

/// Prefix every tag-carrying predicate's key with `"parent_"`, for `Filter::Parent` — mirrors
/// what the old `ParentTag*` variants encoded directly. Non-tag predicates (`Side`/`HasParent`/
/// `Prefix`/`Infix`) pass through unchanged; they have no parent-scoped counterpart.
fn prefix_expr_tags(e: Expr) -> Expr {
    match e {
        Expr::True => Expr::True,
        Expr::False => Expr::False,
        Expr::Lit(Literal::Pos(p)) => Expr::Lit(Literal::Pos(prefix_predicate(p))),
        Expr::Lit(Literal::Neg(p)) => Expr::Lit(Literal::Neg(prefix_predicate(p))),
        Expr::Not(x) => Expr::Not(Box::new(prefix_expr_tags(*x))),
        Expr::And(xs) => Expr::And(xs.into_iter().map(prefix_expr_tags).collect()),
        Expr::Or(xs) => Expr::Or(xs.into_iter().map(prefix_expr_tags).collect()),
    }
}

fn prefix_predicate(p: Predicate) -> Predicate {
    match p {
        Predicate::Eq(k, v) => Predicate::Eq(format!("parent_{k}"), v),
        Predicate::Contains(k, v) => Predicate::Contains(format!("parent_{k}"), v),
        Predicate::StartsWith(k, v) => Predicate::StartsWith(format!("parent_{k}"), v),
        Predicate::EndsWith(k, v) => Predicate::EndsWith(format!("parent_{k}"), v),
        Predicate::Exists(k) => Predicate::Exists(format!("parent_{k}")),
        Predicate::FirstTagIn(ks, vs) => {
            Predicate::FirstTagIn(ks.into_iter().map(|k| format!("parent_{k}")).collect(), vs)
        }
        Predicate::Num(k, op, bits) => Predicate::Num(format!("parent_{k}"), op, bits),
        p @ (Predicate::HasKeyPrefix(_)
        | Predicate::HasParent
        | Predicate::Prefix(_)
        | Predicate::Infix(_)
        | Predicate::Side(_)
        | Predicate::TagsEmpty) => p,
    }
}

pub fn to_nnf(expr: Expr) -> Expr {
    match expr {
        Expr::True => Expr::True,
        Expr::False => Expr::False,
        Expr::Lit(lit) => Expr::Lit(lit),
        Expr::And(exprs) => Expr::And(exprs.into_iter().map(to_nnf).collect()),
        Expr::Or(exprs) => Expr::Or(exprs.into_iter().map(to_nnf).collect()),
        Expr::Not(inner) => match *inner {
            Expr::True => Expr::False,
            Expr::False => Expr::True,
            Expr::Lit(Literal::Pos(p)) => Expr::Lit(Literal::Neg(p)),
            Expr::Lit(Literal::Neg(p)) => Expr::Lit(Literal::Pos(p)),
            Expr::Not(inner2) => to_nnf(*inner2),
            Expr::And(exprs) => Expr::Or(exprs.into_iter().map(|e| to_nnf(Expr::Not(Box::new(e)))).collect()),
            Expr::Or(exprs) => Expr::And(exprs.into_iter().map(|e| to_nnf(Expr::Not(Box::new(e)))).collect()),
        },
    }
}

pub fn to_dnf(expr: Expr) -> Vec<Vec<Literal>> {
    match expr {
        Expr::True => vec![vec![]],
        Expr::False => vec![],
        Expr::Lit(lit) => vec![vec![lit]],
        Expr::Or(exprs) => {
            let mut result = vec![];
            for e in exprs {
                result.extend(to_dnf(e));
            }
            result
        }
        Expr::And(exprs) => {
            if exprs.is_empty() {
                return vec![vec![]];
            }
            let mut result = to_dnf(exprs[0].clone());
            for e in exprs.into_iter().skip(1) {
                let current_dnf = to_dnf(e);
                let mut new_result = vec![];
                for t1 in &result {
                    for t2 in &current_dnf {
                        let mut merged = t1.clone();
                        merged.extend(t2.clone());
                        new_result.push(merged);
                    }
                }
                result = new_result;
                if result.is_empty() {
                    break;
                }
            }
            result
        }
        Expr::Not(_) => unreachable!("Must be NNF before conversion to DNF"),
    }
}

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

    // 3. Side checks (self, left, right) + infix — a term can't fix two different ones
    let mut sides = HashSet::new();
    let mut infixes = HashSet::new();
    for lit in term {
        if let Literal::Pos(Predicate::Side(s)) = lit { sides.insert(s); }
        if let Literal::Pos(Predicate::Infix(i)) = lit { infixes.insert(i); }
    }
    if sides.len() > 1 || infixes.len() > 1 {
        return (false, vec![]);
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

/// Every topic directory holding at least one category kind subfolder, as `(topic name, topic dir)`,
/// sorted by name (skips shared config-root files and any topic without categories). Used by
/// lint + tests.
pub fn topic_category_dirs() -> Vec<(String, PathBuf)> {
    let topics = crate::paths::config_root();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&topics).into_iter().flatten().flatten() {
        let dir = entry.path();
        let has_kind = ElementKind::ALL.iter().any(|k| dir.join(k.subdir()).is_dir());
        if has_kind {
            out.push((entry.file_name().to_string_lossy().into_owned(), dir));
        }
    }
    out.sort();
    out
}

/// Load every topic's per-kind categories, fully macro/sanitizer-resolved (the same
/// `topic::load` pipeline `TopicRunner::load` uses, minus the `outputs`/`transforms.json`/
/// `producers.json` machinery this lint has no use for), and collect overlaps per (topic, kind).
/// Shared entry point.
pub fn find_all_topic_overlaps() -> anyhow::Result<Vec<(String, Vec<Overlap>)>> {
    let mut out = Vec::new();
    for (topic, dir) in topic_category_dirs() {
        let config_root = dir.parent().expect("topics/<name> has parent");
        let sanitizers = load_topic_sanitizers(&dir, config_root)
            .map_err(|e| anyhow::anyhow!("loading {topic} sanitizers: {e}"))?;
        let shared_macros = load_shared_macros(config_root)
            .map_err(|e| anyhow::anyhow!("loading shared macros.json: {e}"))?;
        let topic_macros = load_topic_macros(&dir)
            .map_err(|e| anyhow::anyhow!("loading {topic} macros: {e}"))?;
        let raw_macros = crate::topic::load::merge(&shared_macros, &topic_macros);
        let resolved_macros = resolve_macros(&raw_macros, &sanitizers)
            .map_err(|e| anyhow::anyhow!("resolving {topic} macros: {e}"))?;
        let macros: HashMap<String, Filter> = resolved_macros.iter()
            .map(|(k, v)| Ok((k.clone(), serde_json::from_value(v.clone())?)))
            .collect::<anyhow::Result<_>>()
            .map_err(|e: anyhow::Error| anyhow::anyhow!("parsing {topic} resolved macros: {e}"))?;

        let per_kind = load_topic_categories(&dir, &resolved_macros, &macros, &sanitizers)
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
        let per_topic = find_all_topic_overlaps().expect("loading topic categories");
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
