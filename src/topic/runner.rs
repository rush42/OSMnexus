//! The run phase of a topic: `TopicRunner` wraps a `CompiledTopic` and evaluates it per element.
//! Field access goes through `Deref`, so call sites read compiled data exactly as before; the
//! wrapper is a zero-cost newtype, held by value (no `Arc` on the hot path).

use std::ops::Deref;

use crate::osm::types::{ElementKind, RawTags, WayMeta};
use crate::output::rows::TopicRow;
use crate::topic::compiled::CompiledTopic;
use crate::topic::pipeline::build_topic_rows;

/// A compiled topic ready to process elements.
pub struct TopicRunner(CompiledTopic);

impl Deref for TopicRunner {
    type Target = CompiledTopic;
    fn deref(&self) -> &CompiledTopic {
        &self.0
    }
}

impl From<CompiledTopic> for TopicRunner {
    fn from(compiled: CompiledTopic) -> Self {
        TopicRunner(compiled)
    }
}

impl TopicRunner {
    /// Compile every topic under the config root (see `CompiledTopic::load_all`).
    pub fn load_all(tree_max_depth: usize) -> anyhow::Result<Vec<Self>> {
        Ok(CompiledTopic::load_all(tree_max_depth)?.into_iter().map(TopicRunner).collect())
    }

    /// Compile one topic (see `CompiledTopic::load`).
    pub fn load(name: &str, tree_max_depth: usize) -> anyhow::Result<Self> {
        CompiledTopic::load(name, tree_max_depth).map(TopicRunner)
    }

    pub fn compiled(&self) -> &CompiledTopic {
        &self.0
    }

    /// Run the topic's pipeline for one element of `kind`, handing off to `build_topic_rows`, which
    /// applies `kind`'s own transform pipeline (if any), `exclude_condition`, side-split,
    /// categorize/extract into tag rows against the kind's category set. `raw_tags` are the
    /// element's untouched tags — `build_topic_rows` only clones them if it actually needs to
    /// mutate a copy (an excluded element, or one with no transform pipeline, never pays for it).
    pub fn process<'a>(
        &self,
        kind: ElementKind,
        osm_id: i64,
        raw_tags: &'a RawTags<'a>,
        meta: &WayMeta,
        parents: &[(i64, crate::topic::inherit::ExportedFields)],
    ) -> Vec<TopicRow> {
        if !self.has_kind(kind) {
            return Vec::new();
        }
        build_topic_rows(&self.0, kind, osm_id, raw_tags, meta, parents)
    }}

#[cfg(test)]
mod producer_tree_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::Map;

    use crate::lang::expr::{filter_to_expr, to_nnf, Expr, Literal, Predicate};
    use crate::lang::producer::{ExtractCtx, Producer, Rule};
    use crate::osm::types::RawTags;
    use crate::topic::TopicRunner;

    /// Positive `Eq` atoms in `e` → tag → observed values (mirrors
    /// `decision_tree::tests::collect_pairs`, duplicated here to avoid depending on that module's
    /// private test helper).
    fn collect_pairs(e: &Expr, out: &mut BTreeMap<String, BTreeSet<String>>) {
        match e {
            Expr::Lit(Literal::Pos(Predicate::Eq(extract, v))) => {
                for k in extract.tag_names() {
                    out.entry(k).or_default().insert(v.clone());
                }
            }
            Expr::Lit(_) | Expr::True | Expr::False => {}
            Expr::Not(x) => collect_pairs(x, out),
            Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| collect_pairs(x, out)),
        }
    }

    /// Every `Match`'s `rules` that got a compiled tree, anywhere inside `p` (a rule's own `value`,
    /// or `Parent`'s inner producer, can itself be a further `Match`).
    fn find_compiled_matches<'a>(p: &'a Producer, out: &mut Vec<&'a [Rule]>) {
        match p {
            Producer::Match { rules, tree, .. } => {
                if tree.is_some() {
                    out.push(rules);
                }
                for r in rules {
                    find_compiled_matches(&r.value, out);
                }
            }
            Producer::Parent(inner) => find_compiled_matches(inner, out),
            Producer::Extract { .. } | Producer::Const { .. } => {}
        }
    }

    /// A compiled `Producer::Match` tree must produce the exact same `Produced` (value + annotate)
    /// as a plain linear scan over the same rules, for every object — the tree only prunes
    /// candidates it can prove don't apply (see `decision_tree::build`'s own doc on why
    /// `Producer::Match` needs `assume_match_is_final: false`).
    #[test]
    fn producer_tree_matches_linear() {
        let runner = TopicRunner::load("roads", crate::decision_tree::DEFAULT_TREE_MAX_DEPTH)
            .expect("load roads topic");

        let mut checked_any_field = false;
        for field in runner.default_producers.iter().chain(runner.category_producers.iter().flatten()) {
            let mut compiled = Vec::new();
            find_compiled_matches(&field.source, &mut compiled);
            if compiled.is_empty() {
                continue;
            }
            checked_any_field = true;

            let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for rules in &compiled {
                for r in rules.iter() {
                    collect_pairs(&to_nnf(filter_to_expr(&r.when)), &mut refs);
                }
            }

            let mut linear = field.source.clone();
            linear.clear_trees();

            // Cartesian product over every referenced tag's observed values plus "absent" — small
            // per-tag domains here (a handful of values each), so this stays bounded.
            let keys: Vec<&String> = refs.keys().collect();
            let mut combos: Vec<Vec<Option<&str>>> = vec![vec![]];
            for k in &keys {
                let mut vals: Vec<Option<&str>> = refs[*k].iter().map(|v| Some(v.as_str())).collect();
                vals.push(None);
                let mut next = Vec::with_capacity(combos.len() * vals.len());
                for combo in &combos {
                    for v in &vals {
                        let mut c = combo.clone();
                        c.push(*v);
                        next.push(c);
                    }
                }
                combos = next;
            }

            let mut checked = 0usize;
            for has_parent in [false, true] {
                for combo in &combos {
                    let mut tags: RawTags = RawTags::default();
                    for (k, v) in keys.iter().zip(combo.iter()) {
                        if let Some(v) = v {
                            tags.insert((*k).clone().into(), v.to_string().into());
                        }
                    }
                    let parent_tags = if has_parent { Some(tags.clone()) } else { None };
                    let annotations = Map::new();
                    let ctx = ExtractCtx {
                        obj_tags: &tags,
                        parent_tags: parent_tags.as_ref(),
                        id: "",
                        annotations: &annotations,
                    };
                    let a = field.source.eval(&ctx).map(|p| (p.value, p.annotate));
                    let b = linear.eval(&ctx).map(|p| (p.value, p.annotate));
                    assert_eq!(
                        a, b,
                        "[{}] tree≠linear for tags={tags:?} has_parent={has_parent}",
                        field.output
                    );
                    checked += 1;
                }
            }
            assert!(checked > 0, "[{}] no test cases generated", field.output);
        }
        assert!(checked_any_field, "no compiled Producer::Match trees found to test against");
    }
}

#[cfg(test)]
mod skip_untagged_tests {
    use crate::osm::types::ElementKind;
    use crate::topic::CompiledTopic;

    /// Whichever config the process-global root happens to point at — these tests mutate a loaded
    /// runner rather than loading a specific config, because `paths::CONFIG_ROOT` is a `OnceLock`
    /// (first caller wins) and the test binary runs every test in one process, so a test that set
    /// its own root would silently get whichever config another test set first.
    fn any_runner() -> CompiledTopic {
        let runners = CompiledTopic::load_all(6).expect("loading topics");
        runners.into_iter().next().expect("config has at least one topic")
    }

    /// `accept_all` keeps every element regardless of tags, so untagged nodes must never be
    /// skipped — the case where the optimization has to switch itself off or rows would be lost.
    ///
    /// `exclude_condition` is cleared to isolate `accept_all`: a topic whose exclude filter already
    /// rejects untagged elements is *still* legitimately skippable, so leaving a real config's
    /// filter in place would test the wrong thing (and did, on first writing).
    #[test]
    fn accept_all_never_skips_untagged() {
        let mut r = any_runner();
        r.categories.remove(&ElementKind::Node);
        r.exclude_condition = None;
        r.accept_all.insert(ElementKind::Node);
        assert!(
            !r.probe_skip_untagged().contains(&ElementKind::Node),
            "an accept_all node topic matches untagged nodes, so skipping them would drop rows"
        );
    }

    /// The converse of the above, and the case that motivates probing rather than analysing filter
    /// shapes: an `exclude_condition` satisfied by an untagged element makes the kind skippable even
    /// though `accept_all` would otherwise keep everything.
    #[test]
    fn exclude_condition_can_make_accept_all_skippable() {
        let mut r = any_runner();
        r.categories.remove(&ElementKind::Node);
        r.accept_all.insert(ElementKind::Node);
        // `railway` absent → true for an untagged element, so everything untagged is excluded.
        r.exclude_condition = Some(
            serde_json::from_value(serde_json::json!({ "not": { "tag": "railway", "exists": true } }))
                .expect("filter parses"),
        );
        assert!(r.probe_skip_untagged().contains(&ElementKind::Node));
    }

    /// A kind the topic doesn't handle at all is trivially skippable, and `skips_untagged` reports
    /// that without the probe having to say anything about it.
    #[test]
    fn unhandled_kind_is_skippable() {
        let mut r = any_runner();
        r.categories.remove(&ElementKind::Node);
        r.accept_all.remove(&ElementKind::Node);
        assert!(r.skips_untagged(ElementKind::Node));
    }

    /// The probe must agree with the real pipeline for every kind the loaded config handles: if it
    /// says a kind is skippable, `build_topic_rows` on empty tags really does yield nothing. This is
    /// what makes the optimization safe against a filter that's satisfied by a tag's *absence*
    /// (a negated existence check), which no filter-shape heuristic would catch reliably.
    #[test]
    fn probe_matches_real_pipeline_on_empty_tags() {
        use crate::osm::types::{RawTags, WayMeta};
        for r in CompiledTopic::load_all(6).expect("loading topics") {
            for kind in [ElementKind::Node, ElementKind::Way, ElementKind::Relation] {
                if !r.has_kind(kind) {
                    continue;
                }
                let rows = crate::topic::pipeline::build_topic_rows(
                    &r,
                    kind,
                    0,
                    &RawTags::default(),
                    &WayMeta { timestamp: None, user: None, changeset: None },
                    &[],
                );
                assert_eq!(
                    r.skips_untagged(kind),
                    rows.is_empty(),
                    "[{}] {kind:?}: skips_untagged disagrees with the pipeline on empty tags",
                    r.table()
                );
            }
        }
    }
}
