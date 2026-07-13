use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::topic::load::{load_shared_macros, load_topic_categories};
use crate::tag_engine::categories::CategoriesFile;
use crate::tag_engine::filter::Filter;
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

pub fn filter_to_expr(filter: &Filter, macros: &HashMap<String, Filter>) -> Expr {
    match filter {
        Filter::Bool(true) => Expr::True,
        Filter::Bool(false) => Expr::False,
        Filter::And { and } => Expr::And(and.iter().map(|f| filter_to_expr(f, macros)).collect()),
        Filter::Or { or } => Expr::Or(or.iter().map(|f| filter_to_expr(f, macros)).collect()),
        Filter::Not { not } => Expr::Not(Box::new(filter_to_expr(not, macros))),

        // Every macro resolves to a JSON `Filter` (per-topic `categories/macros.json` + the
        // shared config-root `macros.json`, merged in by `find_all_topic_overlaps`). An unknown
        // name is a config bug — `build_order` already hard-errors on it — so panic rather than
        // model it as an opaque atom.
        Filter::Macro { r#macro } => {
            let mac = macros.get(r#macro)
                .unwrap_or_else(|| panic!("unknown macro '{}' in overlap analysis", r#macro));
            filter_to_expr(mac, macros)
        },

        // `sanitize` is ignored here: the overlap lint is a conservative heuristic and treats a
        // sanitized comparison like a raw one (atoms are independent, which keeps it sound enough).
        Filter::TagEq { tag, eq, .. } => Expr::Lit(Literal::Pos(Predicate::Eq(tag.clone(), eq.clone()))),
        Filter::TagExists { tag, exists: true } => Expr::Lit(Literal::Pos(Predicate::Exists(tag.clone()))),
        Filter::TagExists { tag, exists: false } => Expr::Lit(Literal::Neg(Predicate::Exists(tag.clone()))),
        Filter::TagContains { tag, contains, .. } => Expr::Lit(Literal::Pos(Predicate::Contains(tag.clone(), contains.clone()))),
        Filter::TagStartsWith { tag, starts_with } => Expr::Lit(Literal::Pos(Predicate::StartsWith(tag.clone(), starts_with.clone()))),
        Filter::TagEndsWith { tag, ends_with } => Expr::Lit(Literal::Pos(Predicate::EndsWith(tag.clone(), ends_with.clone()))),
        Filter::TagIn { tag, r#in, .. } => {
            let exprs: Vec<_> = r#in.iter().map(|v| Expr::Lit(Literal::Pos(Predicate::Eq(tag.clone(), v.clone())))).collect();
            Expr::Or(exprs)
        },
        Filter::TagInSet { tag, in_set } => {
            // Expand the named set to an OR of equalities, mirroring TagIn.
            let exprs: Vec<_> = crate::value_sets::value_set(in_set)
                .iter()
                .map(|v| Expr::Lit(Literal::Pos(Predicate::Eq(tag.clone(), v.clone()))))
                .collect();
            Expr::Or(exprs)
        },

        Filter::FirstTagIn { first_tag, r#in, .. } => Expr::Lit(Literal::Pos(Predicate::FirstTagIn(first_tag.clone(), r#in.clone()))),
        Filter::FirstTagInSet { first_tag, in_set, .. } => {
            let mut vals: Vec<String> = crate::value_sets::value_set(in_set).iter().cloned().collect();
            vals.sort();
            Expr::Lit(Literal::Pos(Predicate::FirstTagIn(first_tag.clone(), vals)))
        },
        Filter::FirstTagExists { first_tag, exists: true, .. } => Expr::Lit(Literal::Pos(Predicate::Exists(first_tag[0].clone()))), // approximation
        Filter::FirstTagExists { first_tag, exists: false, .. } => Expr::Lit(Literal::Neg(Predicate::Exists(first_tag[0].clone()))),

        // Recurse as normal, then prefix every tag key in the result with "parent_" — same
        // encoding the old one-off `ParentTag*` variants used, so downstream overlap analysis
        // (which special-cases `parent_`-prefixed keys, e.g. `decision_tree.rs`'s branch-key
        // filter) doesn't need to know `Parent` exists.
        Filter::Parent { parent } => prefix_expr_tags(filter_to_expr(parent, macros)),

        Filter::Side { side } => Expr::Lit(Literal::Pos(Predicate::Side(side.clone()))),
        Filter::Prefix { prefix } => Expr::Lit(Literal::Pos(Predicate::Prefix(prefix.clone()))),
        Filter::Infix { infix } => Expr::Lit(Literal::Pos(Predicate::Infix(infix.clone()))),
        Filter::NumLt  { num, lt,  .. } => Expr::Lit(Literal::Pos(Predicate::Num(num.clone(), NumOp::Lt,  lt.to_bits()))),
        Filter::NumLte { num, lte, .. } => Expr::Lit(Literal::Pos(Predicate::Num(num.clone(), NumOp::Lte, lte.to_bits()))),
        Filter::NumGt  { num, gt,  .. } => Expr::Lit(Literal::Pos(Predicate::Num(num.clone(), NumOp::Gt,  gt.to_bits()))),
        Filter::NumGte { num, gte, .. } => Expr::Lit(Literal::Pos(Predicate::Num(num.clone(), NumOp::Gte, gte.to_bits()))),
        Filter::HasKeyPrefix { has_key_prefix } => Expr::Lit(Literal::Pos(Predicate::HasKeyPrefix(has_key_prefix.clone()))),
        Filter::HasParent { has_parent: true } => Expr::Lit(Literal::Pos(Predicate::HasParent)),
        Filter::HasParent { has_parent: false } => Expr::Lit(Literal::Neg(Predicate::HasParent)),
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
        | Predicate::Side(_)) => p,
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
    let macros = &cats.macros;

    // Precompute each category's DNF, keeping only internally consistent terms.
    let mut category_dnfs: HashMap<String, Vec<Vec<Literal>>> = HashMap::new();
    for cat in &cats.categories {
        let dnf = to_dnf(to_nnf(filter_to_expr(&cat.condition, macros)));
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

/// Load every topic's per-kind categories and collect overlaps per (topic, kind). Shared entry point.
pub fn find_all_topic_overlaps() -> anyhow::Result<Vec<(String, Vec<Overlap>)>> {
    let mut out = Vec::new();
    for (topic, dir) in topic_category_dirs() {
        let per_kind = load_topic_categories(&dir)
            .map_err(|e| anyhow::anyhow!("loading {topic} categories: {e}"))?;
        // Merge shared cross-topic macros so `filter_to_expr` can inline every `Macro` reference,
        // mirroring the runtime merge in `topic_runner`. `dir` is `topics/<name>`.
        let shared = load_shared_macros(dir.parent().expect("topics/<name> has parent"))
            .map_err(|e| anyhow::anyhow!("loading shared macros.json: {e}"))?;
        for (kind, mut cats) in per_kind {
            for (k, v) in &shared {
                cats.macros.entry(k.clone()).or_insert_with(|| v.clone());
            }
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
