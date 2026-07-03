//! Discrimination net over the compiled category `order`, to prune `categorize`'s first-match walk.
//!
//! The tree branches on discriminating equality-tags (chiefly `highway`): descend by the object's
//! tag values to a leaf holding a small, order-preserving subset of the priority list, then run the
//! existing first-match `eval` over just that subset.
//!
//! **Soundness.** The tree only prunes an order-node when it is *provably false* for the branch
//! value; the leaf's authoritative `eval` decides the rest. Pruning uses a three-valued (Kleene)
//! evaluation of the node's NNF condition under a partial assignment: for a branch on tag `T=v`,
//! every atom mentioning `T` is decided concretely (we know the value) and all other atoms are
//! `Unknown`. If the whole expression evaluates to `False`, the node cannot match → prune it. Worst
//! case the tree prunes nothing and matches today's full walk; it never changes results.
//!
//! Only sanitize-free tags are branch keys: a sanitized comparison tests a *derived* value, so
//! branching on the raw tag value would be unsound (and `filter_to_expr` drops the sanitize flag).

use std::collections::HashMap;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::classify::categories::{CategoriesFile, CategoryContext, Filter, OrderedNode};
use crate::classify::overlap::{filter_to_expr, to_nnf, Expr, Literal, NumOp, Predicate};

/// Stop branching past this depth or once a candidate set is this small.
const MAX_DEPTH: usize = 6;
const LEAF_MAX: usize = 2;

#[derive(Debug, Clone)]
pub enum DecisionTree {
    /// Order-node indices (ascending = priority order) to first-match `eval` over.
    Leaf(Vec<usize>),
    Branch {
        tag: String,
        /// Child per enumerated exact-eq value of `tag`.
        children: FxHashMap<String, DecisionTree>,
        /// Object's `tag` absent or not among the enumerated values.
        wildcard: Box<DecisionTree>,
    },
}

impl Default for DecisionTree {
    fn default() -> Self {
        DecisionTree::Leaf(Vec::new())
    }
}

impl DecisionTree {
    /// Descend to the leaf's candidate node-index slice for this object.
    pub fn candidates<'a>(&'a self, ctx: &CategoryContext) -> &'a [usize] {
        let mut node = self;
        loop {
            match node {
                DecisionTree::Leaf(idxs) => return idxs,
                DecisionTree::Branch { tag, children, wildcard } => {
                    node = ctx
                        .tags
                        .get(tag)
                        .and_then(|v| children.get(v.as_str()))
                        .unwrap_or(wildcard);
                }
            }
        }
    }
}

/// Build the discrimination net from a topic's compiled `order` (+ categories/macros for conditions).
pub fn build(cats: &CategoriesFile) -> DecisionTree {
    // NNF condition per order node (index-aligned with `cats.order`).
    let exprs: Vec<Expr> = cats
        .order
        .iter()
        .map(|n| to_nnf(filter_to_expr(node_condition(cats, n), &cats.macros)))
        .collect();

    // Tags used anywhere with a `sanitize` chain are ineligible as branch keys.
    let mut banned: FxHashSet<String> = FxHashSet::default();
    for n in &cats.order {
        collect_sanitized_tags(node_condition(cats, n), &cats.macros, &mut banned);
    }

    let all: Vec<usize> = (0..cats.order.len()).collect();
    build_rec(&exprs, &banned, all, &mut FxHashSet::default(), 0)
}

fn node_condition<'a>(cats: &'a CategoriesFile, n: &'a OrderedNode) -> &'a Filter {
    match n {
        OrderedNode::Category { idx } => &cats.categories[*idx].condition,
        OrderedNode::Skip { condition } => condition,
    }
}

fn build_rec(
    exprs: &[Expr],
    banned: &FxHashSet<String>,
    candidates: Vec<usize>,
    used: &mut FxHashSet<String>,
    depth: usize,
) -> DecisionTree {
    if candidates.len() <= LEAF_MAX || depth >= MAX_DEPTH {
        return DecisionTree::Leaf(candidates);
    }
    let Some(tag) = choose_branch_tag(exprs, banned, &candidates, used) else {
        return DecisionTree::Leaf(candidates);
    };

    let values = eligible_values(exprs, &candidates, &tag);
    used.insert(tag.clone());

    let mut children = FxHashMap::default();
    for v in &values {
        let kept: Vec<usize> =
            candidates.iter().copied().filter(|&i| keep_for_value(&exprs[i], &tag, v)).collect();
        children.insert(v.clone(), build_rec(exprs, banned, kept, used, depth + 1));
    }
    let wild: Vec<usize> =
        candidates.iter().copied().filter(|&i| keep_for_wildcard(&exprs[i], &tag)).collect();
    let wildcard = Box::new(build_rec(exprs, banned, wild, used, depth + 1));

    used.remove(&tag);
    DecisionTree::Branch { tag, children, wildcard }
}

/// Pick the eligible branch tag whose worst-case child (or wildcard) is smallest, provided it
/// prunes at all. `None` if no eligible tag reduces the candidate set.
fn choose_branch_tag(
    exprs: &[Expr],
    banned: &FxHashSet<String>,
    candidates: &[usize],
    used: &FxHashSet<String>,
) -> Option<String> {
    // Candidate branch tags: plain (non-parent) tags appearing in a positive Eq atom.
    let mut tags: FxHashSet<String> = FxHashSet::default();
    for &i in candidates {
        collect_eq_tags(&exprs[i], &mut |k| {
            if !k.starts_with("parent_") && !banned.contains(k) && !used.contains(k) {
                tags.insert(k.to_string());
            }
        });
    }

    let mut best: Option<String> = None;
    let mut best_worst = candidates.len(); // require strictly smaller than the full set
    for tag in tags {
        let values = eligible_values(exprs, candidates, &tag);
        let mut worst = candidates.iter().filter(|&&i| keep_for_wildcard(&exprs[i], &tag)).count();
        for v in &values {
            let c = candidates.iter().filter(|&&i| keep_for_value(&exprs[i], &tag, v)).count();
            worst = worst.max(c);
        }
        if worst < best_worst {
            best_worst = worst;
            best = Some(tag);
        }
    }
    best
}

/// All exact-eq values of `tag` across the candidate conditions.
fn eligible_values(exprs: &[Expr], candidates: &[usize], tag: &str) -> Vec<String> {
    let mut vals: FxHashSet<String> = FxHashSet::default();
    for &i in candidates {
        collect_eq_values(&exprs[i], tag, &mut |v| {
            vals.insert(v.to_string());
        });
    }
    let mut out: Vec<String> = vals.into_iter().collect();
    out.sort();
    out
}

fn keep_for_value(expr: &Expr, tag: &str, v: &str) -> bool {
    kleene(expr, &|p| decide_value(p, tag, v)) != K::F
}

fn keep_for_wildcard(expr: &Expr, tag: &str) -> bool {
    kleene(expr, &|p| decide_wildcard(p, tag)) != K::F
}

// ── Three-valued (Kleene) evaluation ─────────────────────────────────────────────

#[derive(PartialEq, Eq, Clone, Copy)]
enum K {
    F,
    U,
    T,
}

fn not_k(k: K) -> K {
    match k {
        K::F => K::T,
        K::U => K::U,
        K::T => K::F,
    }
}

fn kleene(e: &Expr, decide: &impl Fn(&Predicate) -> K) -> K {
    match e {
        Expr::True => K::T,
        Expr::False => K::F,
        Expr::Lit(Literal::Pos(p)) => decide(p),
        Expr::Lit(Literal::Neg(p)) => not_k(decide(p)),
        Expr::Not(inner) => not_k(kleene(inner, decide)),
        // AND = min (any F ⇒ F, else any U ⇒ U, else T).
        Expr::And(xs) => xs.iter().fold(K::T, |acc, x| min_k(acc, kleene(x, decide))),
        // OR = max (any T ⇒ T, else any U ⇒ U, else F).
        Expr::Or(xs) => xs.iter().fold(K::F, |acc, x| max_k(acc, kleene(x, decide))),
    }
}

fn min_k(a: K, b: K) -> K {
    if a == K::F || b == K::F { K::F } else if a == K::U || b == K::U { K::U } else { K::T }
}
fn max_k(a: K, b: K) -> K {
    if a == K::T || b == K::T { K::T } else if a == K::U || b == K::U { K::U } else { K::F }
}

/// Decide a predicate under "object's `tag` == `v`, everything else unknown". Atoms on `tag` are
/// fully decidable (we know the value); all others are `Unknown`.
fn decide_value(p: &Predicate, tag: &str, v: &str) -> K {
    let b = |cond: bool| if cond { K::T } else { K::F };
    match p {
        Predicate::Eq(k, w) if k == tag => b(w == v),
        Predicate::Exists(k) if k == tag => K::T,
        Predicate::Contains(k, s) if k == tag => b(v.contains(s.as_str())),
        Predicate::StartsWith(k, s) if k == tag => b(v.starts_with(s.as_str())),
        Predicate::EndsWith(k, s) if k == tag => b(v.ends_with(s.as_str())),
        Predicate::Num(k, op, bits) if k == tag => match v.trim().parse::<f64>() {
            Ok(n) => b(num_cmp(n, op, f64::from_bits(*bits))),
            Err(_) => K::F,
        },
        _ => K::U,
    }
}

/// Decide a predicate under "object's `tag` is absent or not among the enumerated eq-values".
/// Every positive Eq on `tag` in the candidate set uses an enumerated value, so all are false here;
/// existence and value-shape atoms stay unknown (the object could carry an un-enumerated value).
fn decide_wildcard(p: &Predicate, tag: &str) -> K {
    match p {
        Predicate::Eq(k, _) if k == tag => K::F,
        _ => K::U,
    }
}

fn num_cmp(n: f64, op: &NumOp, thr: f64) -> bool {
    match op {
        NumOp::Lt => n < thr,
        NumOp::Lte => n <= thr,
        NumOp::Gt => n > thr,
        NumOp::Gte => n >= thr,
    }
}

// ── Expr walkers ─────────────────────────────────────────────────────────────────

/// Invoke `f` on the key of every positive `Eq` atom.
fn collect_eq_tags(e: &Expr, f: &mut impl FnMut(&str)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(k, _))) => f(k),
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_eq_tags(x, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_eq_tags(x, f)),
    }
}

/// Invoke `f` on the value of every positive `Eq` atom on `tag`.
fn collect_eq_values(e: &Expr, tag: &str, f: &mut impl FnMut(&str)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(k, v))) if k == tag => f(v),
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_eq_values(x, tag, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_eq_values(x, tag, f)),
    }
}

/// Collect plain tag names compared through a `sanitize` chain (recursing into macros). Such tags
/// are ineligible as branch keys — the comparison tests a derived value, not the raw tag.
fn collect_sanitized_tags(f: &Filter, macros: &HashMap<String, Filter>, out: &mut FxHashSet<String>) {
    match f {
        Filter::And { and } => and.iter().for_each(|c| collect_sanitized_tags(c, macros, out)),
        Filter::Or { or } => or.iter().for_each(|c| collect_sanitized_tags(c, macros, out)),
        Filter::Not { not } => collect_sanitized_tags(not, macros, out),
        Filter::Macro { r#macro } => {
            if let Some(m) = macros.get(r#macro) {
                collect_sanitized_tags(m, macros, out);
            }
        }
        Filter::TagEq { tag, sanitize: Some(_), .. } => {
            out.insert(tag.clone());
        }
        Filter::TagIn { tag, sanitize: Some(_), .. } => {
            out.insert(tag.clone());
        }
        Filter::NumLt { num, sanitize: Some(_), .. }
        | Filter::NumLte { num, sanitize: Some(_), .. }
        | Filter::NumGt { num, sanitize: Some(_), .. }
        | Filter::NumGte { num, sanitize: Some(_), .. } => {
            out.insert(num.clone());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    use crate::classify::categories::{
        categorize, categorize_linear, load_categories_from_dir, load_shared_macros, CategoryContext,
    };
    use crate::classify::overlap::topic_category_dirs;
    use crate::classify::sanitize::SanitizerRegistry;
    use crate::osm::types::RawTags;
    use crate::output::types::Side;

    use super::{filter_to_expr, to_nnf, Expr, Literal, Predicate};

    /// Positive Eq (plain-tag) atoms across every order-node condition → tag → observed values.
    fn referenced_eq(cats: &crate::classify::categories::CategoriesFile) -> BTreeMap<String, BTreeSet<String>> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for n in &cats.order {
            let cond = match n {
                crate::classify::categories::OrderedNode::Category { idx } => &cats.categories[*idx].condition,
                crate::classify::categories::OrderedNode::Skip { condition } => condition,
            };
            let e = to_nnf(filter_to_expr(cond, &cats.macros));
            collect_pairs(&e, &mut out);
        }
        out
    }

    fn collect_pairs(e: &Expr, out: &mut BTreeMap<String, BTreeSet<String>>) {
        match e {
            Expr::Lit(Literal::Pos(Predicate::Eq(k, v))) if !k.starts_with("parent_") => {
                out.entry(k.clone()).or_default().insert(v.clone());
            }
            Expr::Lit(_) | Expr::True | Expr::False => {}
            Expr::Not(x) => collect_pairs(x, out),
            Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_pairs(x, out)),
        }
    }

    /// The tree-pruned `categorize` must return the same category as the full linear walk for every
    /// object we can construct — the tree only drops provably-false nodes.
    #[test]
    fn tree_matches_linear() {
        let sanitizers = SanitizerRegistry::new(HashMap::new());

        for (topic, dir) in topic_category_dirs() {
            let mut cats = load_categories_from_dir(&dir).expect("load categories");
            let shared = dir.parent().and_then(|p| p.parent()).unwrap().join("_shared");
            for (k, v) in load_shared_macros(&shared).expect("shared macros") {
                cats.macros.entry(k).or_insert(v);
            }
            cats.build_order().expect("build order + tree");

            let refs = referenced_eq(&cats);
            let hw_vals: Vec<Option<String>> = {
                let mut v: Vec<Option<String>> =
                    refs.get("highway").into_iter().flatten().map(|s| Some(s.clone())).collect();
                v.push(Some("zzz_unknown".to_string())); // an un-enumerated value → wildcard
                v.push(None); // highway absent
                v
            };
            // Single "other" (tag,value) perturbations, plus the no-perturbation case.
            let mut others: Vec<Option<(String, String)>> = vec![None];
            for (t, vals) in &refs {
                if t == "highway" {
                    continue;
                }
                for val in vals {
                    others.push(Some((t.clone(), val.clone())));
                }
            }

            let sides = [Side::Self_, Side::Left, Side::Right];
            let mut checked = 0usize;
            for hw in &hw_vals {
                for other in &others {
                    for &side in &sides {
                        let mut tags: RawTags = RawTags::default();
                        if let Some(h) = hw {
                            tags.insert("highway".into(), h.clone());
                        }
                        if let Some((t, v)) = other {
                            tags.insert(t.clone(), v.clone());
                        }
                        let (prefix, parent_highway, parent_tags): (Option<&str>, Option<&str>, Option<&RawTags>) =
                            match side {
                                Side::Self_ => (None, None, None),
                                _ => (Some("cycleway"), Some("secondary"), None),
                            };
                        let ctx = CategoryContext {
                            tags: &tags,
                            side,
                            prefix,
                            parent_highway,
                            parent_tags,
                            infix: None,
                            sanitizers: &sanitizers,
                        };
                        let a = categorize(&ctx, &cats).map(|c| c.id.clone());
                        let b = categorize_linear(&ctx, &cats).map(|c| c.id.clone());
                        assert_eq!(
                            a, b,
                            "[{topic}] tree≠linear for highway={hw:?} other={other:?} side={side:?}"
                        );
                        checked += 1;
                    }
                }
            }
            assert!(checked > 0, "[{topic}] no test cases generated");
        }
    }
}
