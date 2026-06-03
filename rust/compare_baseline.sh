#!/usr/bin/env bash
# Self-diff the current `osm` DB output against the *_base snapshot tables.
# Expects the pipeline to have been run already. Prints mismatch/missing/extra
# per topic per column. 0 everywhere = byte-identical.
set -euo pipefail
DB=osm
for t in bikelanes roads barrierlines; do
  echo "── $t ──"
  psql -d "$DB" -tA <<SQL
SELECT 'mismatch_osm     ' || count(*) FROM $t c JOIN ${t}_base b USING(id) WHERE c.osm      IS DISTINCT FROM b.osm;
SELECT 'mismatch_derived ' || count(*) FROM $t c JOIN ${t}_base b USING(id) WHERE c.derived  IS DISTINCT FROM b.derived;
SELECT 'mismatch_private ' || count(*) FROM $t c JOIN ${t}_base b USING(id) WHERE c.private  IS DISTINCT FROM b.private;
SELECT 'mismatch_meta    ' || count(*) FROM $t c JOIN ${t}_base b USING(id) WHERE c.meta     IS DISTINCT FROM b.meta;
SELECT 'mismatch_minzoom ' || count(*) FROM $t c JOIN ${t}_base b USING(id) WHERE c.minzoom  IS DISTINCT FROM b.minzoom;
SELECT 'missing (in base)' || count(*) FROM ${t}_base b LEFT JOIN $t c USING(id) WHERE c.id IS NULL;
SELECT 'extra   (in new) ' || count(*) FROM $t c LEFT JOIN ${t}_base b USING(id) WHERE b.id IS NULL;
SQL
done
