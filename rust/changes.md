# Unterschiede zur Lua-Pipeline

Bekannte Abweichungen zwischen der Rust-Implementierung und dem Lua/osm2pgsql-Original.
Stand: Berlin-Vergleich, ~39k Bikelane-Rows, **0 falsch klassifizierte Rows** (gleiche ID, anderes Category).
Gesamtabweichung **+132** Rows — ausschließlich durch neue OSM-Ways seit dem letzten Lua-Run.

## Bewusst nicht implementiert

Diese Features existieren in Lua, werden in Rust nicht repliziert:

| Feature | Lua-Datei | Auswirkung |
|---|---|---|
| `is_sidepath`-CSV-Estimation | `pseudo_tags_sidepath/` | ~220 Ways landen in `_adjoiningOrIsolated` statt `_isolated` |
| Mapillary-Coverage-CSV | `pseudo_tags_mapillary_coverage/` | `mapillary_coverage` immer `null` |
| `todos_lines`-Tabelle | `BikelaneTodos.lua` | Tabelle existiert nicht |
| `bikelanesPresence`-Tabelle | `BikelanesPresence.lua` | Tabelle existiert nicht |
| `bikeSuitability`-Tabelle | `BikeSuitability.lua` | Tabelle existiert nicht |
| `roadsPathClasses`-Tabelle | `roads_bikelanes.lua` | Paths/Footways landen in `roads` |
| SQL-Post-Processing | `2_move_bikelanes.sql` | Kein `ST_OffsetCurve` für left/right Geometrien |

## Bekannte Abweichungen in der Klassifizierung

Aktuelle Diffs für Berlin: **0 misclassified rows** (gleiche Way-ID, anderes Category).

Noch vorhandene Zähler-Differenzen (Rust > Lua) sind ausschließlich auf unterschiedliche
Datenbasis zurückzuführen (Rust-Lauf mit neuerem PBF-Stand als Lua-DB).

Alte bekannte Abweichungen durch `_is_sidepath`-CSV (nicht implementiert):
- `cycleway_adjoining`: ±6 Rows — CSv ordnet diese in Rust als `_adjoiningOrIsolated` ein
- `footwayBicycleYes_adjoiningOrIsolated`: +17 — ohne CSV kein `_adjoining`-Zuordnung
- `needsClarification`: ~64 — Spiegeleffekt

## Behobene Lua-Portierungsfehler (Chronologie)

### 11. Category-IDs für `cyclewaySeparated_*`
**Problem**: JSON-Dateien hießen `cyclewaySeparated_adjoining.json` etc., erzeugte IDs
`cyclewaySeparated_adjoining`. Lua verwendet `id = 'cycleway'` → `cycleway_adjoining`.  
**Fix**: Dateien umbenannt zu `cycleway_adjoining.json` etc.; alle `excludes`-Referenzen
in anderen JSON-Dateien aktualisiert.  
**Auswirkung**: Category-Namen im DB-Output stimmten nicht mit Lua überein.

### 12. Build.rs: nicht-deterministische Category-Reihenfolge
**Problem**: `fs::read_dir()` liefert Filesystem-Reihenfolge (inode-abhängig). Obwohl
`excludes` die meisten Priority-Konflikte löst, ist eine stabile Reihenfolge wichtig.  
**Fix**: `build.rs` sortiert Dateien vor dem Einlesen alphabetisch.

### 13. `footAndCyclewaySegregated` Edge-Case: falsche Separations-Bedingung
**Problem**: Die JSON-Condition für den `traffic_mode:right=foot`-Fall prüfte rohe
Tag-Werte, aber Lua normalisiert via `SANITIZE_ROAD_TAGS`. Beispiel:
`separation:right=kerb;tree_row` → Lua: `tree_row` (blockierend) → Rust: nicht in Liste →
fälschlich als nicht-blockierend gewertet. 11 Ways wurden als `cycleway_adjoining`
statt `footAndCyclewaySegregated_adjoining` klassifiziert.  
**Fix**: Neues Rust-Prädikat `is_foot_and_cycleway_segregated_edge_case` in
`bikelane_categories.rs` mit vollständiger Normalisierungslogik.

### 14. `cyclewayOnHighwayProtected`: fehlender Counter-Flow-Fall + falsche Guards
**Problem**: Lua prüft drei Fälle: (1) physische Trennung links + kein Motor-Vehicle
rechts + kein segregated, (2) Parken links + kein segregated, (3) Counter-Flow:
Motor-Vehicle rechts + physische Trennung rechts. JSON hatte nur Fälle 1 und 2
(unvollständig), und: (a) Kein `NOT traffic_mode_right=motor_vehicle`-Guard in Fall 1,
(b) `segregated ~= nil` falsch als `NOT in [yes, no]` statt `exists: false`,
(c) `motorized` wurde nicht zu `motor_vehicle` normalisiert.  
**Fix**: Neues Rust-Prädikat `is_protected_bikelane_separation` mit vollständiger
Normalisierungslogik für `traffic_mode` (inkl. `motorized→motor_vehicle`) und
`separation` (inkl. Compound-Werte wie `tree_row;kerb→tree_row`). 1 Way wurde
als `cycleway_adjoining` statt `cyclewayOnHighwayProtected` klassifiziert.

### 1. `highway` fehlte in transformierten Side-Objekten
**Problem**: `obj.tags` enthielt kein `highway`-Feld für left/right-Objekte → alle
Category-Conditions die `highway` prüften feuerten nie.  
**Fix**: `side_split.rs` — `obj.tags.insert("highway", transformation.highway)` vor dem Push.  
**Auswirkung**: ~18k fehlende Bikelane-Rows (left/right-Objekte aus bare `cycleway=*` Tags).

### 2. `allow_bare_prefix: false` blockierte bare Cycleway-Tags
**Problem**: `cycleway=lane` auf `highway=secondary` ohne `:left`/`:right`-Suffix
erzeugte keine Side-Objekte.  
**Fix**: `side_split.rs` — bare Prefix wird immer verarbeitet (wie in Lua).  
**Auswirkung**: ~18k fehlende Rows, zusammen mit Fix 1.

### 3. `infix` wurde nicht getrackt
**Problem**: `NOT_EXPECTED`-Kategorie prüft `_infix == ""` (Bare-Prefix-Fall),
war nie erreichbar weil `infix` immer `None`.  
**Fix**: `side_split.rs` — `TransformedObject.infix` trackt welcher Infix gewonnen hat.

### 4. `IsSidepath(tags)` Override durch `is_sidepath=no` fehlte
**Problem**: Lua gibt `is_sidepath=no` Priorität über `_parent_highway` (truthy).
In Rust ignorierte `is_sidepath()` den `=no`-Fall.  
**Fix**: `bikelane_categories.rs` — early return `false` wenn `is_sidepath=no`.

### 5. `cyclewaySeparated_base`: Lua-Truthy-Check für `is_sidepath`
**Problem**: Lua prüft `tags.is_sidepath` als truthy (jeder non-nil Wert, auch `"no"`).
Rust prüfte nur `== "yes"` → Ways mit `is_sidepath=no` wurden nicht als
`cyclewaySeparated` erkannt und landeten in `needsClarification`.  
**Fix**: `bikelane_categories.rs` — `tag(ctx, "is_sidepath").is_some()` statt `tag_is(..., "yes")`.

### 6. `footAndCyclewayShared`: Gemischte bicycle/foot-Werte
**Problem**: Lua erlaubt nur gleiche Wertpaare (`designated+designated` oder `yes+yes`).
Rust akzeptierte `bicycle=yes + foot=designated` (gemischt) → falsche Klassifizierung.  
**Fix**: `bikelane_categories.rs` — Bedingung explizit auf gleiche Paare eingeschränkt.

### 7. `footAndCyclewaySegregated`: Gleiche gemischte Wertpaare
**Problem**: Identisch zu Fix 6, aber in der Segregated-Variante.  
**Fix**: `bikelane_categories.rs` — gleiche Korrektur.

### 8. `traffic_mode_right`-Edge-Case: falscher Tag-Key
**Problem**: Lua liest `tags['traffic_mode:right']` (Doppelpunkt). Rust prüfte
`traffic_mode_right` (Unterstrich) → Edge-Case für `footAndCyclewaySegregated` feuerte nie.  
**Fix**: `bikelane_categories.rs` — korrekter Key `traffic_mode:right`.

### 9. `separation:right=surface` → `"no"` Normalisierung fehlte
**Problem**: Lua normalisiert `separation:right=surface` via `SANITIZE_ROAD_TAGS.separation`
zu `"no"` (Farbe ≠ physische Trennung). Rust verglich literal → `"surface" != "no"` →
Edge-Case blockiert.  
**Fix**: `bikelane_categories.rs` — `"surface"` und `"lane_separator"` werden zu `"no"` normalisiert.

### 10. BetweenLanes Dual-Tagging Guard fehlte
**Problem**: Lua filtert Side-Objekte aus `cyclewayOnHighway_advisory/exclusive` wenn der
Parent-Way `cycleway:lanes=*|lane|*` hat (Doppel-Tagging "Radweg in Mittellage" + Schutzstreifen).
Rust ignorierte diesen Guard → ~45 falsch klassifizierte Advisory-Rows.  
**Fix**: `bikelane_categories.rs` — `has_between_lanes_conditions()` + Suffix-Check in
`is_advisory_or_exclusive()`.

## Architektonische Unterschiede

### Tags: `osm` vs. `derived` Trennung
Lua speichert alle Werte in einer einzigen `tags`-JSONB-Spalte (interne `_`-Prefix-Tags
werden vor dem Schreiben gefiltert). Rust trennt explizit:
- `osm`: rohe OSM-Tag-Werte (was im PBF stand)
- `derived`: berechnete Werte (category, side, road, length_m, lifecycle)

### Kein `_`-Prefix-Hack
Lua nutzt `_side`, `_parent`, `_infix`, `_prefix` als interne Tags im selben Map.
Rust nutzt `CategoryContext` — ein typisierter Struct der diese Felder explizit trennt.

### Kategorien: JSON-Definitionen
Lua: geordnete Liste in `BikelaneCategories.lua`, First-Match gewinnt.
Rust: Jede Kategorie ist eine eigene JSON-Datei in `src/classify/categories/`.
Gemeinsame Macros in `src/classify/macros.json`. Build.rs kompiliert alles zu
`categories_compiled.json`. Reihenfolge ist alphabetisch nach Dateiname.

Statt fester Prioritätsreihenfolge nutzen die JSON-Kategorien `excludes`-Listen:
eine Kategorie wird übersprungen, wenn eine dort gelistete Kategorie ebenfalls greift.
Komplexe Bedingungen (Normalisierung, numerische Vergleiche) sind als Rust-Prädikate
in `bikelane_categories.rs` implementiert und per `{"macro": "name"}` referenziert.
