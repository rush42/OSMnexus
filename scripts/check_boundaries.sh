#!/usr/bin/env bash
#
# Architecture fitness function (see `boundaries_plan.md`, step 8): the functional core must not
# depend on the imperative shell. Fails if a core module imports a shell module, touches the
# filesystem, or holds process-global state — except for the explicitly listed, known leaks that
# later steps remove. Shrink ALLOWED as those steps land; never grow it.
#
# Usage: scripts/check_boundaries.sh   (exit 0 = clean)

set -euo pipefail
cd "$(dirname "$0")/.."

# Core = the future `osmnexus-lang` crate.
core=(
    src/lang
    src/categorize
    src/decision_tree
    src/topic/spec.rs
    src/topic/load.rs
    src/topic/model.rs
    src/topic/schema.rs
    src/value_sets.rs
    src/units.rs
    src/traffic.rs
)

# Shell modules the core must not import, plus I/O and global-state markers.
forbidden='crate::(osm::reader|geom|output|db|processing|csv_source|config|main|topic::(runner|pipeline|inherit)|paths)\b|std::fs\b|fs::read|read_dir|OnceLock|static [A-Z_]+: (Atomic|OnceLock|Mutex|RwLock)'

# Known leaks, as "file:pattern" (grep -E on the matching line). Each names the step that removes it.
ALLOWED=(
    # step 6: config root + file reads move to the shell (ConfigSources / CompileEnv)
    'src/topic/load.rs:std::fs|fs::read|read_dir|crate::paths'
    'src/topic/model.rs:std::fs|fs::read|read_dir|crate::paths'
    'src/categorize/linter.rs:std::fs|fs::read|read_dir|crate::paths'
    'src/categorize/transform.rs:std::fs|fs::read|read_dir|crate::paths'
    'src/decision_tree/optimal.rs:std::fs|fs::read|read_dir|crate::paths'
    'src/value_sets.rs:OnceLock|std::fs|fs::read|crate::paths'
    'src/units.rs:OnceLock|std::fs|fs::read|crate::paths'
    'src/traffic.rs:static LEFT_HAND_TRAFFIC'
    # the empty-tags probe runs the real pipeline at compile time; revisit when the pipeline crate
    # exists (move the probe there, or move build_topic_rows into the core)
    'src/topic/model.rs:crate::topic::pipeline'
    'src/topic/model.rs:crate::osm::types'
    # an immutable empty map constant, not config state (no input flows into it)
    'src/lang/producer.rs:OnceLock'
    # `#[cfg(test)]` helpers that load real configs
    'src/decision_tree/tests.rs:.'
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
