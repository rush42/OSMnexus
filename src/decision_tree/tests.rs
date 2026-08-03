use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::topic::load::{
    load_shared_macros, load_topic_categories, load_topic_macros, load_topic_sanitizers, merge, resolve_macros,
};
use crate::categorize::categories::{categorize, categorize_linear, CategoriesFile, OrderedNode};
use crate::lang::filter::Filter;
use crate::lang::producer::ExtractCtx;
use crate::categorize::linter::{filter_to_expr, to_nnf, topic_category_dirs, Expr, Literal, Predicate};
use serde_json::{Map, Value};
use crate::osm::types::RawTags;

/// Positive Eq (plain-tag) atoms across every order-node condition → tag → observed values.
fn referenced_eq(cats: &CategoriesFile) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for n in &cats.order {
        let cond = match n {
            OrderedNode::Category { idx } => &cats.categories[*idx].condition,
            OrderedNode::Skip { condition } => condition,
        };
        let e = to_nnf(filter_to_expr(cond));
        collect_pairs(&e, &mut out);
    }
    out
}

fn collect_pairs(e: &Expr, out: &mut BTreeMap<String, BTreeSet<String>>) {
    match e {
        Expr::Lit(Literal::Pos(Predicate::Eq(extract, v))) => {
            for k in extract.tag_names().into_iter().filter(|k| !k.starts_with("parent_")) {
                out.entry(k).or_default().insert(v.clone());
            }
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
      let config_root = dir.parent().unwrap();
      let shared_macros = load_shared_macros(config_root).expect("shared macros");
      let sanitizers = load_topic_sanitizers(&dir, config_root).expect("load sanitizers");
      let topic_macros = load_topic_macros(&dir).expect("topic macros");
      let raw_macros = merge(&shared_macros, &topic_macros);
      let resolved_macros = resolve_macros(&raw_macros).expect("resolve macros");
      let macros: HashMap<String, Filter> = resolved_macros.iter()
          .map(|(k, v)| Ok((k.clone(), serde_json::from_value(crate::topic::load::inline_sanitize_refs(v.clone(), &sanitizers)?)?)))
          .collect::<anyhow::Result<_>>()
          .expect("parse resolved macros");
      for (kind, mut cats) in load_topic_categories(&dir, &resolved_macros, &macros, &sanitizers).expect("load categories") {
        let topic = format!("{topic}/{}", kind.subdir());
        cats.build_order(crate::config::DEFAULT_TREE_MAX_DEPTH, false).expect("build order + tree");

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
            [("highway".into(), "secondary".into())].into_iter().collect();
        let mut checked = 0usize;
        for hw in &hw_vals {
            for other in &others {
                for &side in &sides {
                    let mut tags: RawTags = RawTags::default();
                    if let Some(h) = hw {
                        tags.insert("highway".into(), h.clone().into());
                    }
                    if let Some((t, v)) = other {
                        tags.insert(t.clone().into(), v.clone().into());
                    }
                    let (prefix, side_parent_tags): (Option<&str>, Option<&RawTags<'_>>) = match side {
                        "self" => (None, None),
                        _ => (Some("cycleway"), Some(&parent_tags)),
                    };
                    let mut annotations = Map::new();
                    annotations.insert("_side".to_owned(), Value::String(side.to_owned()));
                    if let Some(p) = prefix {
                        annotations.insert("_prefix".to_owned(), Value::String(p.to_owned()));
                    }
                    let ctx = ExtractCtx {
                        obj_tags: &tags,
                        parent_tags: side_parent_tags,
                        id: "",
                        annotations: &annotations,
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
