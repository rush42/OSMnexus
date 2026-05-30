use anyhow::Result;
use std::collections::{HashMap, HashSet};

use osm_bikelanes::classify::bikelane_categories::get_categories;
use osm_bikelanes::classify::overlap::{filter_to_expr, to_nnf, to_dnf, Literal, Predicate};

/// Returns (is_consistent, warnings)
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
                return (false, vec![]); // Eq("a") AND Not(Eq("a")) is handled by exact contradiction usually, but just in case
            }
            // If eq_val doesn't match starts_with
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

    // 3. Side checks (self, left, right)
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

fn check_overlaps() {
    let cat_data = get_categories();
    let macros = &cat_data.macros;

    let mut category_dnfs = HashMap::new();

    for cat in &cat_data.categories {
        let expr = filter_to_expr(&cat.condition, macros);
        let nnf = to_nnf(expr.clone());
        let mut dnf = to_dnf(nnf);

        // Retain only internally consistent terms for each category
        dnf.retain(|term| check_term_consistency(term).0);
        category_dnfs.insert(cat.id.clone(), dnf);
    }

    let cat_names: Vec<String> = cat_data.categories.iter().map(|c| c.id.clone()).collect();
    let mut found_any = false;

    for i in 0..cat_names.len() {
        for j in (i + 1)..cat_names.len() {
            let cat_a = &cat_names[i];
            let cat_b = &cat_names[j];
            let dnf_a = &category_dnfs[cat_a];
            let dnf_b = &category_dnfs[cat_b];

            let mut overlaps = false;
            let mut all_warnings = Vec::new();

            for t_a in dnf_a {
                for t_b in dnf_b {
                    let mut combined = t_a.clone();
                    combined.extend(t_b.clone());

                    let (consistent, warnings) = check_term_consistency(&combined);
                    if consistent {
                        overlaps = true;
                        if !warnings.is_empty() {
                            all_warnings.extend(warnings);
                        }
                    }
                }
            }

            if overlaps {
                found_any = true;
                println!("{}  <->  {}", cat_a, cat_b);
                all_warnings.sort();
                all_warnings.dedup();
                for w in all_warnings {
                    println!("  Warning: {}", w);
                }
            }
        }
    }

    if !found_any {
        println!("No overlaps found.");
    }
}

fn main() -> Result<()> {
    check_overlaps();
    Ok(())
}
