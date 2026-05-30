# Unterschiede zur Lua-Pipeline

Bekannte Abweichungen zwischen der Rust-Implementierung und dem Lua/osm2pgsql-Original.
Stand: Berlin-Vergleich, ~39k Bikelane-Rows, Gesamtabweichung **+135** (0.35%).

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

Aktuelle Diffs für Berlin (Lua-Basis vs. Rust):

```
cyclewaySeparated_adjoining          +18   (Lua=5915, Rust=5933)
footwayBicycleYes_adjoiningOrIsolated +17  (Lua=2218, Rust=2235)
needsClarification                   +64   Spiegeleffekt der Abweichungen oben
```

Ursache: Lua verwendet den `_is_sidepath`-CSV-Pseudo-Tag als Tiebreaker zwischen
`_adjoining` / `_adjoiningOrIsolated`. Ohne CSV fallen betroffene Ways in Rust
in die falsche Subkategorie oder in `needsClarification`.

## Behobene Lua-Portierungsfehler (Chronologie)

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

### Kategorien: `CATEGORY_DEFINITIONS` Static Array
Lua: geordnete Liste in `BikelaneCategories.lua`, First-Match gewinnt.
Rust: `static CATEGORY_DEFINITIONS: &[&BikelaneCategory]` — identische Reihenfolge,
compile-time-bekannt. Jede Kategorie ist eine `const BikelaneCategory` mit `condition: fn`.
