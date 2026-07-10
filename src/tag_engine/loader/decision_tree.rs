//! Build-time compilation of a `CategoriesFile`'s compiled `order` into the `DecisionTree`
//! discrimination net (`tag_engine::producer::decision_tree`) that prunes `categorize`'s
//! first-match walk. See that module's doc comment for the soundness argument; this file is just
//! the compiler — nothing here runs per-object.

use std::collections::HashMap;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::tag_engine::producer::categories::{CategoriesFile, OrderedNode};
use crate::tag_engine::producer::decision_tree::{
    num_cmp, DecisionTree, HAS_PARENT_KEY, INFIX_KEY, PREFIX_KEY, SIDE_KEY,
};
use crate::tag_engine::producer::filter::Filter;
use crate::lint::{filter_to_expr, to_nnf, Expr, Literal, Predicate};

/// Stop branching once a candidate set shrinks to this size — below this, the eval cost of a leaf
/// walk is already cheap, so a further branch's overhead isn't worth the diminishing prune.
const LEAF_MAX: usize = 2;

/// An order-node index paired with its condition *as simplified so far* along this build path —
/// each branch constant-folds the just-decided fact into every surviving candidate's expression
/// (see `simplify`), so later branches (and the leaf) see a strictly smaller residual, and several
/// branches can jointly resolve an `Or` that no single one of them could decide alone.
type Candidate = (usize, Expr);

/// Build the discrimination net from a topic's compiled `order` (+ categories/macros for
/// conditions). `max_depth` caps how many tags deep a branch chain may go (see `build_rec`).
pub fn build(cats: &CategoriesFile, max_depth: usize) -> DecisionTree {
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

    let all: Vec<Candidate> = exprs.into_iter().enumerate().collect();
    build_rec(&banned, all, &mut FxHashSet::default(), &mut FxHashSet::default(), 0, max_depth)
}

fn node_condition<'a>(cats: &'a CategoriesFile, n: &'a OrderedNode) -> &'a Filter {
    match n {
        OrderedNode::Category { idx } => &cats.categories[*idx].condition,
        OrderedNode::Skip { condition } => condition,
    }
}

fn build_rec(
    banned: &FxHashSet<String>,
    candidates: Vec<Candidate>,
    used: &mut FxHashSet<String>,
    used_atoms: &mut FxHashSet<Predicate>,
    depth: usize,
    max_depth: usize,
) -> DecisionTree {
    if candidates.len() <= LEAF_MAX || depth >= max_depth {
        return DecisionTree::Leaf(candidates.into_iter().map(|(i, _)| i).collect());
    }
    let Some(choice) = choose_branch(banned, &candidates, used, used_atoms) else {
        return DecisionTree::Leaf(candidates.into_iter().map(|(i, _)| i).collect());
    };

    match choice {
        BranchChoice::Key(tag) => {
            let values = eligible_values(&candidates, &tag);
            used.insert(tag.clone());

            let mut children = FxHashMap::default();
            for v in &values {
                let kept = fold_candidates(&candidates, &|p| decide_value(p, &tag, v));
                children.insert(
                    v.clone(),
                    build_rec(banned, kept, used, used_atoms, depth + 1, max_depth),
                );
            }
            let wild = fold_candidates(&candidates, &|p| decide_wildcard(p, &tag));
            let wildcard = Box::new(build_rec(banned, wild, used, used_atoms, depth + 1, max_depth));

            used.remove(&tag);
            DecisionTree::Branch { tag, children, wildcard }
        }
        BranchChoice::Atom(atom) => {
            used_atoms.insert(atom.clone());

            let b = |cond: bool| if cond { K::T } else { K::F };
            let on_true = fold_candidates(&candidates, &|p| if p == &atom { b(true) } else { K::U });
            let on_false = fold_candidates(&candidates, &|p| if p == &atom { b(false) } else { K::U });
            let on_true = Box::new(build_rec(banned, on_true, used, used_atoms, depth + 1, max_depth));
            let on_false =
                Box::new(build_rec(banned, on_false, used, used_atoms, depth + 1, max_depth));

            used_atoms.remove(&atom);
            DecisionTree::AtomBranch { atom, on_true, on_false }
        }
    }
}

/// Constant-fold every candidate's current expression under `decide`, dropping any that collapse
/// to `Expr::False` and keeping the (possibly smaller) simplified expression for the survivors.
fn fold_candidates(candidates: &[Candidate], decide: &impl Fn(&Predicate) -> K) -> Vec<Candidate> {
    candidates
        .iter()
        .filter_map(|(i, e)| {
            let simplified = simplify(e, decide);
            (simplified != Expr::False).then_some((*i, simplified))
        })
        .collect()
}

enum BranchChoice {
    Key(String),
    Atom(Predicate),
}

/// Pick the branch (tag/context key, or single atom) whose worst-case child is smallest, provided
/// it prunes at all. `None` if nothing reduces the candidate set.
fn choose_branch(
    banned: &FxHashSet<String>,
    candidates: &[Candidate],
    used: &FxHashSet<String>,
    used_atoms: &FxHashSet<Predicate>,
) -> Option<BranchChoice> {
    let mut best: Option<BranchChoice> = None;
    let mut best_worst = candidates.len(); // require strictly smaller than the full set

    // Candidate branch keys: plain (non-parent) tags appearing in a positive Eq atom, plus the
    // `side`/`has_parent`/`prefix`/`infix` sentinels wherever those predicates appear at all.
    let mut tags: FxHashSet<String> = FxHashSet::default();
    for (_, e) in candidates {
        collect_branch_keys(e, &mut |k| {
            if !k.starts_with("parent_") && !banned.contains(k) && !used.contains(k) {
                tags.insert(k.to_string());
            }
        });
    }
    for tag in tags {
        let values = eligible_values(candidates, &tag);
        let mut worst =
            candidates.iter().filter(|(_, e)| keep_for(e, &|p| decide_wildcard(p, &tag))).count();
        for v in &values {
            let c = candidates.iter().filter(|(_, e)| keep_for(e, &|p| decide_value(p, &tag, v))).count();
            worst = worst.max(c);
        }
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Key(tag));
        }
    }

    // Candidate atoms: total (non-Eq) predicates on a plain, unsanitized tag — `Contains`,
    // `StartsWith`, `EndsWith`, `Num`, `Exists`, `HasKeyPrefix` — either polarity, since a missing
    // tag decides them outright (false) rather than leaving them unknown.
    let mut atoms: FxHashSet<Predicate> = FxHashSet::default();
    for (_, e) in candidates {
        collect_atom_candidates(e, banned, &mut |p| {
            if !used_atoms.contains(p) {
                atoms.insert(p.clone());
            }
        });
    }
    for atom in atoms {
        let b = |cond: bool| if cond { K::T } else { K::F };
        let t = candidates
            .iter()
            .filter(|(_, e)| keep_for(e, &|p| if p == &atom { b(true) } else { K::U }))
            .count();
        let f = candidates
            .iter()
            .filter(|(_, e)| keep_for(e, &|p| if p == &atom { b(false) } else { K::U }))
            .count();
        let worst = t.max(f);
        if worst < best_worst {
            best_worst = worst;
            best = Some(BranchChoice::Atom(atom));
        }
    }

    best
}

/// All exact-eq values of `tag` across the candidate conditions. For the `side`/`has_parent`
/// sentinels the domain is fixed and known up front (no need to scan conditions for it).
fn eligible_values(candidates: &[Candidate], tag: &str) -> Vec<String> {
    match tag {
        SIDE_KEY => return vec!["left".to_string(), "right".to_string(), "self".to_string()],
        HAS_PARENT_KEY => return vec!["false".to_string(), "true".to_string()],
        _ => {}
    }
    let mut vals: FxHashSet<String> = FxHashSet::default();
    for (_, e) in candidates {
        collect_eq_values(e, tag, &mut |v| {
            vals.insert(v.to_string());
        });
    }
    let mut out: Vec<String> = vals.into_iter().collect();
    out.sort();
    out
}

fn keep_for(expr: &Expr, decide: &impl Fn(&Predicate) -> K) -> bool {
    kleene(expr, decide) != K::F
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

/// Constant-fold `e` under `decide`: replace every literal `decide` can resolve with `True`/`False`
/// and normalize `Not`/`And`/`Or` (an `And` with a `False` child collapses to `False`, an `Or` with
/// a `True` child collapses to `True`, singleton `And`/`Or` unwrap). This is what lets several
/// branches jointly resolve an `Or` none of them could decide alone: each branch's fact gets baked
/// into the candidate's expression, so it's already gone by the time a later branch or leaf looks.
fn simplify(e: &Expr, decide: &impl Fn(&Predicate) -> K) -> Expr {
    match e {
        Expr::True | Expr::False => e.clone(),
        Expr::Lit(Literal::Pos(p)) => match decide(p) {
            K::T => Expr::True,
            K::F => Expr::False,
            K::U => e.clone(),
        },
        Expr::Lit(Literal::Neg(p)) => match decide(p) {
            K::T => Expr::False,
            K::F => Expr::True,
            K::U => e.clone(),
        },
        Expr::Not(inner) => match simplify(inner, decide) {
            Expr::True => Expr::False,
            Expr::False => Expr::True,
            other => Expr::Not(Box::new(other)),
        },
        Expr::And(xs) => {
            let mut kept = Vec::new();
            for x in xs {
                match simplify(x, decide) {
                    Expr::False => return Expr::False,
                    Expr::True => {}
                    other => kept.push(other),
                }
            }
            match kept.len() {
                0 => Expr::True,
                1 => kept.into_iter().next().unwrap(),
                _ => Expr::And(kept),
            }
        }
        Expr::Or(xs) => {
            let mut kept = Vec::new();
            for x in xs {
                match simplify(x, decide) {
                    Expr::True => return Expr::True,
                    Expr::False => {}
                    other => kept.push(other),
                }
            }
            match kept.len() {
                0 => Expr::False,
                1 => kept.into_iter().next().unwrap(),
                _ => Expr::Or(kept),
            }
        }
    }
}

/// Decide a predicate under "object's `tag` == `v`, everything else unknown". Atoms on `tag` are
/// fully decidable (we know the value); all others are `Unknown`.
fn decide_value(p: &Predicate, tag: &str, v: &str) -> K {
    let b = |cond: bool| if cond { K::T } else { K::F };
    match tag {
        SIDE_KEY => return match p { Predicate::Side(s) => b(s == v), _ => K::U },
        HAS_PARENT_KEY => return match p { Predicate::HasParent => b(v == "true"), _ => K::U },
        PREFIX_KEY => return match p { Predicate::Prefix(s) => b(s == v), _ => K::U },
        INFIX_KEY => return match p { Predicate::Infix(s) => b(s == v), _ => K::U },
        _ => {}
    }
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
    match tag {
        SIDE_KEY => return match p { Predicate::Side(_) => K::F, _ => K::U },
        HAS_PARENT_KEY => return match p { Predicate::HasParent => K::F, _ => K::U },
        PREFIX_KEY => return match p { Predicate::Prefix(_) => K::F, _ => K::U },
        INFIX_KEY => return match p { Predicate::Infix(_) => K::F, _ => K::U },
        _ => {}
    }
    match p {
        Predicate::Eq(k, _) if k == tag => K::F,
        _ => K::U,
    }
}

// ── Expr walkers ─────────────────────────────────────────────────────────────────

/// Invoke `f` on the key of every positive `Eq` atom, and on the relevant sentinel key for every
/// `Side`/`HasParent`/`Prefix`/`Infix` atom (either polarity — their fixed, small domain makes them
/// decidable regardless of sign, unlike an arbitrary-string tag `Eq`/`Neq`).
fn collect_branch_keys(e: &Expr, f: &mut impl FnMut(&str)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(k, _))) => f(k),
        Expr::Lit(Literal::Pos(Predicate::Side(_)) | Literal::Neg(Predicate::Side(_))) => f(SIDE_KEY),
        Expr::Lit(Literal::Pos(Predicate::HasParent) | Literal::Neg(Predicate::HasParent)) => {
            f(HAS_PARENT_KEY)
        }
        Expr::Lit(Literal::Pos(Predicate::Prefix(_)) | Literal::Neg(Predicate::Prefix(_))) => {
            f(PREFIX_KEY)
        }
        Expr::Lit(Literal::Pos(Predicate::Infix(_)) | Literal::Neg(Predicate::Infix(_))) => {
            f(INFIX_KEY)
        }
        Expr::Lit(_) | Expr::True | Expr::False => {}
        Expr::Not(x) => collect_branch_keys(x, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_branch_keys(x, f)),
    }
}

/// Invoke `f` on every `Contains`/`StartsWith`/`EndsWith`/`Num`/`Exists`/`HasKeyPrefix` atom (either
/// polarity) whose tag (if any) is plain and unsanitized — the atom kinds `AtomBranch` can use.
fn collect_atom_candidates(e: &Expr, banned: &FxHashSet<String>, f: &mut impl FnMut(&Predicate)) {
    match e {
        Expr::Lit(Literal::Pos(p) | Literal::Neg(p)) => {
            let tag = match p {
                Predicate::Contains(k, _)
                | Predicate::StartsWith(k, _)
                | Predicate::EndsWith(k, _)
                | Predicate::Exists(k)
                | Predicate::Num(k, _, _) => Some(k.as_str()),
                Predicate::HasKeyPrefix(_) => None,
                _ => return,
            };
            if let Some(tag) = tag {
                if tag.starts_with("parent_") || banned.contains(tag) {
                    return;
                }
            }
            f(p);
        }
        Expr::True | Expr::False => {}
        Expr::Not(x) => collect_atom_candidates(x, banned, f),
        Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_atom_candidates(x, banned, f)),
    }
}

/// Invoke `f` on the value of every positive `Eq` atom on `tag` (or, for `PREFIX_KEY`/`INFIX_KEY`,
/// every positive `Prefix`/`Infix` atom's literal value).
fn collect_eq_values(e: &Expr, tag: &str, f: &mut impl FnMut(&str)) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(k, v))) if k == tag => f(v),
        Expr::Lit(Literal::Pos(Predicate::Prefix(v))) if tag == PREFIX_KEY => f(v),
        Expr::Lit(Literal::Pos(Predicate::Infix(v))) if tag == INFIX_KEY => f(v),
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

    use crate::tag_engine::loader::{load_shared_macros, load_topic_categories, load_topic_sanitizers};
    use crate::tag_engine::producer::categories::{categorize, categorize_linear, CategoriesFile, OrderedNode};
    use crate::tag_engine::producer::filter::Filter;
    use crate::tag_engine::producer::ExtractCtx;
    use crate::lint::{filter_to_expr, to_nnf, topic_category_dirs, Expr, Literal, Predicate};
    use crate::osm::types::RawTags;

    /// Positive Eq (plain-tag) atoms across every order-node condition → tag → observed values.
    fn referenced_eq(cats: &CategoriesFile) -> BTreeMap<String, BTreeSet<String>> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for n in &cats.order {
            let cond = match n {
                OrderedNode::Category { idx } => &cats.categories[*idx].condition,
                OrderedNode::Skip { condition } => condition,
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
        for (topic, dir) in topic_category_dirs() {
          let shared = dir.parent().unwrap().join("_shared");
          let shared_macros = load_shared_macros(&shared).expect("shared macros");
          let sanitizers = load_topic_sanitizers(&dir, &shared).expect("load sanitizers");
          for (kind, mut cats) in load_topic_categories(&dir).expect("load categories") {
            let topic = format!("{topic}/{}", kind.subdir());
            let mut raw_macros = shared_macros.clone();
            for (k, v) in &cats.macros {
                raw_macros.insert(k.clone(), v.clone()); // topic-local overrides shared
            }
            let expanded: HashMap<String, Filter> = raw_macros.iter()
                .map(|(k, v)| Ok((k.clone(), v.expand(&raw_macros, &sanitizers)?)))
                .collect::<anyhow::Result<_>>()
                .expect("expand macros");
            cats.macros = expanded.clone();
            for cat in &mut cats.categories {
                cat.condition = cat.condition.expand(&expanded, &sanitizers).expect("expand category condition");
            }
            cats.build_order(crate::config::DEFAULT_TREE_MAX_DEPTH).expect("build order + tree");

            let refs = referenced_eq(&cats);
            let hw_vals: Vec<Option<String>> = {
                let mut v: Vec<Option<String>> =
                    refs.get("highway").into_iter().flatten().map(|s| Some(s.clone())).collect();
                v.push(Some("zzz_unknown".to_string())); // an un-enumerated value → wildcard
                v.push(None); // highway absent
                v
            };
            let mut others: Vec<Option<(String, String)>> = vec![None];
            for (t, vals) in &refs {
                if t == "highway" {
                    continue;
                }
                for val in vals {
                    others.push(Some((t.clone(), val.clone())));
                }
            }

            let sides = ["self", "left", "right"];
            let parent_tags: RawTags =
                [("highway".to_owned(), "secondary".to_owned())].into_iter().collect();
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
                        let (prefix, side_parent_tags): (Option<&str>, Option<&RawTags>) = match side {
                            "self" => (None, None),
                            _ => (Some("cycleway"), Some(&parent_tags)),
                        };
                        let ctx = ExtractCtx {
                            obj_tags: &tags,
                            parent_tags: side_parent_tags,
                            obj_side: side,
                            prefix,
                            infix: None,
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
}
