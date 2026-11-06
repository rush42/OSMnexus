#!/usr/bin/env bash
#
# Behaviour-preservation check for refactors (see `boundaries_plan.md` step 0).
#
# Captures everything a refactor of the lang/codegen/pipeline boundary could change, into one
# directory of plain files, so two runs can be compared with `diff -r`:
#
#   * codegen: `export_ts_types` for every topic, and `tree_json` for every mode × name
#     (deriver / decision-tree / sanitizer, every category of every kind) — the editor's contract
#   * pipeline: `--output geojsonseq` per PBF, stored as a sha256 of each topic's *sorted*
#     feature lines (sorted because thread scheduling may reorder lines; content must not change)
#
# Usage:
#   scripts/regress.sh <out-dir> [config-dir] [pbf ...]
#   diff -r <out-dir-before> <out-dir-after>     # must print nothing
#
# Defaults: config-dir `configs/tilda`, pbf `editor/fixtures/tiny.osm.pbf`. Pass a Berlin extract
# as an extra pbf for a realistic check. Builds with the `dev-fast` profile (see CLAUDE.md);
# output is identical across profiles, only speed differs.

set -euo pipefail

out=${1:?usage: regress.sh <out-dir> [config-dir] [pbf ...]}
cfg=${2:-configs/tilda}
shift $(( $# >= 2 ? 2 : 1 ))
pbfs=("$@")
[ ${#pbfs[@]} -eq 0 ] && pbfs=(editor/fixtures/tiny.osm.pbf)

cargo build --quiet --profile dev-fast --bin osmnexus --bin tree_json --bin export_ts_types
bin=target/dev-fast

mkdir -p "$out/codegen" "$out/pipeline"

topics=()
for d in "$cfg"/*/; do
    [ -f "$d/topic.json" ] && topics+=("$(basename "$d")")
done

names() { python3 -c 'import json,sys; print("\n".join(json.load(sys.stdin)["names"]))'; }

for t in "${topics[@]}"; do
    "$bin/export_ts_types" "$cfg" "$t" > "$out/codegen/$t.ts"
    for mode in deriver decision-tree sanitizer; do
        "$bin/tree_json" "$cfg" "$t" "$mode" list > "$out/codegen/$t.$mode.list.json"
        while IFS= read -r n; do
            [ -z "$n" ] && continue
            "$bin/tree_json" "$cfg" "$t" "$mode" "$n" > "$out/codegen/$t.$mode.${n//\//_}.json"
        done < <(names < "$out/codegen/$t.$mode.list.json")
    done
    "$bin/tree_json" "$cfg" "$t" category list > "$out/codegen/$t.category.list.json"
    while IFS= read -r kind; do
        [ -z "$kind" ] && continue
        "$bin/tree_json" "$cfg" "$t" category "$kind" > "$out/codegen/$t.category.$kind.json"
        count=$(python3 -c 'import json,sys; print(len(json.load(sys.stdin)["names"]))' \
            < "$out/codegen/$t.category.$kind.json")
        for ((i = 0; i < count; i++)); do
            "$bin/tree_json" "$cfg" "$t" category "$kind" "$i" > "$out/codegen/$t.category.$kind.$i.json"
        done
    done < <(names < "$out/codegen/$t.category.list.json")
done

for pbf in "${pbfs[@]}"; do
    name=$(basename "$pbf" .osm.pbf)
    tmp=$(mktemp -d)
    "$bin/osmnexus" "$pbf" --config-dir "$cfg" --output geojsonseq --out-dir "$tmp" > "$tmp/log" 2>&1 \
        || { cat "$tmp/log"; exit 1; }
    for f in "$tmp"/*.geojsonseq; do
        topic=$(basename "$f" .geojsonseq)
        printf '%s  %s\n' "$(LC_ALL=C sort "$f" | sha256sum | cut -d' ' -f1)" "$(wc -l < "$f")" \
            > "$out/pipeline/$name.$topic.sha256"
    done
    rm -rf "$tmp"
done

echo "wrote $(find "$out" -type f | wc -l) files to $out"
