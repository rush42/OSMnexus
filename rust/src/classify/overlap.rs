use std::collections::HashMap;
use crate::classify::categories::Filter;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Predicate {
    Eq(String, String),
    Contains(String, String),
    StartsWith(String, String),
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
    RustMacro(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NumOp { Lt, Lte, Gt, Gte }

impl Predicate {
    pub fn tags_involved(&self) -> Vec<String> {
        match self {
            Predicate::Eq(k, _) => vec![k.clone()],
            Predicate::Contains(k, _) => vec![k.clone()],
            Predicate::StartsWith(k, _) => vec![k.clone()],
            Predicate::Exists(k) => vec![k.clone()],
            Predicate::FirstTagIn(ks, _) => ks.clone(),
            Predicate::Num(k, _, _) => vec![k.clone()],
            Predicate::HasKeyPrefix(p) => vec![format!("prefix({})", p)],
            Predicate::HasParent => vec!["[parent]".to_string()],
            Predicate::Prefix(_) => vec!["[prefix]".to_string()],
            Predicate::Infix(_) => vec!["[infix]".to_string()],
            Predicate::Side(_) => vec!["[side]".to_string()],
            Predicate::RustMacro(m) => vec![format!("[macro]{}", m)],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Literal {
    Pos(Predicate),
    Neg(Predicate),
}

#[derive(Debug, Clone)]
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
        Filter::And { and } => Expr::And(and.iter().map(|f| filter_to_expr(f, macros)).collect()),
        Filter::Or { or } => Expr::Or(or.iter().map(|f| filter_to_expr(f, macros)).collect()),
        Filter::Not { not } => Expr::Not(Box::new(filter_to_expr(not, macros))),

        Filter::Macro { r#macro } => {
            if let Some(mac) = macros.get(r#macro) {
                filter_to_expr(mac, macros)
            } else {
                // E.g., is_sidepath etc. which are Rust-implemented
                Expr::Lit(Literal::Pos(Predicate::RustMacro(r#macro.clone())))
            }
        },

        Filter::TagEq { tag, eq } => Expr::Lit(Literal::Pos(Predicate::Eq(tag.clone(), eq.clone()))),
        Filter::TagExists { tag, exists: true } => Expr::Lit(Literal::Pos(Predicate::Exists(tag.clone()))),
        Filter::TagExists { tag, exists: false } => Expr::Lit(Literal::Neg(Predicate::Exists(tag.clone()))),
        Filter::TagContains { tag, contains } => Expr::Lit(Literal::Pos(Predicate::Contains(tag.clone(), contains.clone()))),
        Filter::TagStartsWith { tag, starts_with } => Expr::Lit(Literal::Pos(Predicate::StartsWith(tag.clone(), starts_with.clone()))),
        Filter::TagIn { tag, r#in } => {
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

        // FirstTagEq was removed, skipping
        Filter::FirstTagIn { first_tag, r#in } => Expr::Lit(Literal::Pos(Predicate::FirstTagIn(first_tag.clone(), r#in.clone()))),
        Filter::FirstTagExists { first_tag, exists: true } => Expr::Lit(Literal::Pos(Predicate::Exists(first_tag[0].clone()))), // approximation
        Filter::FirstTagExists { first_tag, exists: false } => Expr::Lit(Literal::Neg(Predicate::Exists(first_tag[0].clone()))),

        Filter::ParentTagEq { parent_tag, eq } => Expr::Lit(Literal::Pos(Predicate::Eq(format!("parent_{}", parent_tag), eq.clone()))),
        Filter::ParentTagContains { parent_tag, contains } => Expr::Lit(Literal::Pos(Predicate::Contains(format!("parent_{}", parent_tag), contains.clone()))),
        Filter::ParentTagStartsWith { parent_tag, starts_with } => Expr::Lit(Literal::Pos(Predicate::StartsWith(format!("parent_{}", parent_tag), starts_with.clone()))),
        Filter::ParentTagIn { parent_tag, r#in } => {
            let exprs: Vec<_> = r#in.iter().map(|v| Expr::Lit(Literal::Pos(Predicate::Eq(format!("parent_{}", parent_tag), v.clone())))).collect();
            Expr::Or(exprs)
        },

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
