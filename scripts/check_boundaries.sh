#!/usr/bin/env bash
#
# Architecture fitness function (see `boundaries_plan.md`, step 8): the functional core
# (`crates/osmnexus-lang`) must stay pure. Cargo already makes importing the shell impossible
# (the shell depends on the core, not the other way round); this additionally fails if the core
# touches the filesystem or holds process-global state, except for the listed exceptions. Shrink
# ALLOWED over time; never grow it without a reason next to the entry.
#
# Usage: scripts/check_boundaries.sh   (exit 0 = clean)

set -euo pipefail
cd "$(dirname "$0")/.."

# Core = the future `osmnexus-lang` crate.
core=(crates/osmnexus-lang/src)

# Shell modules the core must not import, plus I/O and global-state markers.
forbidden='crate::(osm::reader|geom|output|db|processing|csv_source|config|config_dir|main|topic::(pipeline|inherit)|paths)\b|std::fs\b|fs::read|\bread_dir\(|OnceLock|static [A-Z_]+: (Atomic|OnceLock|Mutex|RwLock)'

# Known exceptions, as "file:pattern" (grep -E on the matching line), each with its reason.
ALLOWED=(
    # an immutable empty map constant, not config state (no input flows into it)
    'crates/osmnexus-lang/src/lang/producer.rs:OnceLock'
    # `#[cfg(test)]` code: tests read real configs from the repository
    'crates/osmnexus-lang/src/test_support.rs:.'
    'crates/osmnexus-lang/src/decision_tree/optimal.rs:.'
)

violations=0
while IFS= read -r hit; do
    file=${hit%%:*}
    rest=${hit#*:}; line=${rest#*:}
    # skip comments
    [[ "$line" =~ ^[[:space:]]*// ]] && continue
    ok=0
    for a in "${ALLOWED[@]}"; do
        afile=${a%%:*}; apat=${a#*:}
        if [[ "$file" == "$afile" ]] && grep -qE "$apat" <<< "$line"; then ok=1; break; fi
    done
    if (( ! ok )); then
        echo "boundary violation: $hit"
        violations=$((violations + 1))
    fi
done < <(grep -rnE "$forbidden" "${core[@]}" --include=*.rs || true)

if (( violations )); then
    echo "$violations boundary violation(s): the core must not depend on the shell (boundaries_plan.md)"
    exit 1
fi
echo "boundaries OK"
