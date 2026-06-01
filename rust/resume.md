# Rust Bikelane Pipeline — Struktur

Reimplementierung der `roads_bikelanes` Lua/osm2pgsql-Pipeline in Rust.
Ersetzt: `processing/topics/roads_bikelanes/` (Lua + SQL).
Schreibt in dieselben PostgreSQL-Tabellen (`bikelanes`, `roads`).

## Ausführung

```bash
PGDATABASE=osm PGUSER=rush42 ./target/release/osm-pipeline /tmp/berlin-latest.osm.pbf
```

Umgebungsvariablen: `PGHOST`, `PGDATABASE`, `PGUSER`, `PGPASSWORD`, `PGPORT`, `PBF_FILE`.

## Geschwindigkeit

Berlin (94 MB PBF, 466k Ways): **~8 Sekunden** vs. 71 Sekunden mit Lua/osm2pgsql.

## Pipeline-Ablauf

```
PBF-Datei
  ↓
[osm/reader.rs]  Pass 1 (parallel): Highway-Ways + referenzierte Node-IDs sammeln
                 Pass 2 (parallel): Node-Koordinaten auflösen
                 Resolve (parallel, rayon): Geometrien zusammenbauen
  ↓
[main.rs]        Way-Verarbeitung (rayon par_iter):
                   1. Tag-Transformationen anwenden
                   2. Ausschlusskriterien prüfen
                   3. Road-Row bauen → roads-Tabelle
                   4. Side-Split → transformed objects (self/left/right)
                   5. Kategorisierung → bikelanes-Tabelle
  ↓
[db/writer.rs]   PostgreSQL COPY (streaming, producer-consumer)
  ↓
[db/schema.rs]   Indexes erstellen
```

## Modulstruktur

```
src/
├── main.rs                    # Einstieg, Pipeline-Orchestrierung, rayon/async-Brücke
├── config.rs                  # CLI-Args + Env-Vars (clap)
├── error.rs                   # PipelineError (thiserror)
│
├── osm/
│   ├── reader.rs              # PBF-Reader: 2 parallele Passes + Geometrie-Auflösung
│   └── types.rs               # OsmWay, WayMeta, RawTags
│
├── transform/
│   ├── lifecycle.rs           # highway=construction + construction=X → lifecycle=construction
│   ├── opposite.rs            # cycleway=opposite_* → cycleway:left=* Schema
│   ├── construction_prefix.rs # construction:cycleway:left=X → cycleway:left=X
│   ├── cycleway_both.rs       # cycleway=no → cycleway:both=no
│   └── side_split.rs          # Center-Line-Split: Way → [self, left?, right?]
│                              # Port von transformations.lua (GetTransformedObjects)
│
├── classify/
│   ├── highway_classes.rs     # Sets: major/minor/path/sidepath highway values
│   ├── exclude.rs             # Ausschlusskriterien (access, service, indoor, area)
│   ├── bikelane_categories.rs # 29 Kategorien + CategoryContext
│   │                          # Port von BikelaneCategories.lua (CategorizeBikelane)
│   ├── road_classification.rs # road-Wert pro Way (primary, service_alley, …)
│   └── minzoom.rs             # bikelane_minzoom + road_minzoom
│
├── output/
│   ├── types.rs               # BikelaneOsmTags, BikelaneDerived, RoadOsmTags,
│   │                          # RoadDerived, OsmMeta, Side
│   ├── geometry.rs            # WGS84→EPSG:3857, Haversine-Länge, EWKB-Encoding
│   ├── bikelane_row.rs        # BikelaneRow Struct
│   └── road_row.rs            # RoadRow Struct
│
└── db/
    ├── pool.rs                # deadpool-postgres Verbindungspool (Unix-Socket-Support)
    ├── schema.rs              # CREATE TABLE, TRUNCATE, DROP/CREATE INDEX
    └── writer.rs              # PostgreSQL COPY CSV (write_bikelane_csv_row, write_road_csv_row)
```

## Datenbankschema

Beide Tabellen identisch aufgebaut:

```sql
CREATE TABLE bikelanes (
  osm_id   bigint,
  osm_type text,           -- immer "W" (Way)
  id       text NOT NULL,  -- "way/123" oder "way/123/cycleway/left"
  osm      jsonb,          -- rohe OSM-Tags (BikelaneOsmTags)
  derived  jsonb,          -- berechnete Werte (BikelaneDerived)
  meta     jsonb,          -- OSM-Metadaten (timestamp, user, changeset)
  geom     geometry(LineString, 3857),
  minzoom  integer NOT NULL
);
```

`osm` und `derived` sind bewusst getrennt — OSM-Rohdaten vs. Pipeline-Output.

## Parallelisierung

- **PBF-Lesen**: `osmpbf::ElementReader::par_map_reduce` — verarbeitet PBF-Blöcke parallel
- **Way-Verarbeitung**: `rayon::par_iter` über alle Ways
- **DB-Schreiben**: Producer (rayon-Thread) + Consumer (async tokio) über `mpsc::channel`
- **Keine Locks nötig**: Jeder Way ist unabhängig; Ergebnisse werden nach der Verarbeitung gesammelt

## Abhängigkeiten

| Crate | Zweck |
|---|---|
| `osmpbf` | PBF-Datei lesen |
| `rayon` | Parallelverarbeitung |
| `tokio` + `tokio-postgres` | Async DB-Verbindung |
| `deadpool-postgres` | Connection-Pooling |
| `geo` | LineString, Haversine-Länge |
| `serde` + `serde_json` | JSONB-Serialisierung |
| `clap` | CLI + Env-Var-Parsing |
| `chrono` | Timestamp-Formatierung |
| `hex` | EWKB-Hex-Encoding |
| `bytes` + `futures` | COPY-Sink-API |

---

## OSM-Datendateien

| Datei | Größe | Ort |
|---|---|---|
| Berlin (aktuell) | 94 MB | `/tmp/berlin-latest.osm.pbf` |
| Berlin (kleiner Testausschnitt, Prenzlauer Berg) | 3 MB | `/tmp/berlin-small.osm.pbf` |
| Deutschland | 4.5 GB | `/home/rush42/Documents/tilda-geo/rust/germany-latest.osm.pbf` |

Download-Quelle: https://download.geofabrik.de

---

## Martin Tile Server starten

Martin ist als statisch gelinktes Binary unter `/tmp/martin` vorhanden (v1.10.1).

```bash
# Starten (Unix-Socket, Web UI aktiviert)
/tmp/martin \
  --listen-addresses 0.0.0.0:3000 \
  --webui enable-for-all \
  "postgresql://rush42@%2Fvar%2Frun%2Fpostgresql/osm?sslmode=disable"

# Web UI im Browser
open http://localhost:3000

# Tile-URLs
# http://localhost:3000/bikelanes/{z}/{x}/{y}
# http://localhost:3000/roads/{z}/{x}/{y}

# Neustart nach Pipeline-Run
pkill martin; sleep 1 && /tmp/martin ...
```

Martin erkennt alle Tabellen mit einer `geometry`-Spalte automatisch.

---

## Alte Lua/osm2pgsql-Pipeline

Voraussetzungen: `osm2pgsql` installiert (`sudo apt-get install -y osm2pgsql`), Lua-Dependencies gepatcht (s.u.).

### Einmalig: Dependencies patchen

```bash
REPO=/home/rush42/Documents/tilda-geo

# inspect.lua (Lua debug-Lib)
curl -sL https://raw.githubusercontent.com/kikito/inspect.lua/master/inspect.lua \
  -o $REPO/processing/inspect.lua

# ftcsv + penlight stubs (CSV-Loader, Dateien fehlen sowieso)
echo 'local ftcsv={}; function ftcsv.parse() return {} end; return ftcsv' \
  > $REPO/processing/ftcsv.lua
mkdir -p $REPO/processing/pl
echo 'return { exists = function() return false end }' > $REPO/processing/pl/path.lua
echo 'return { size = function() return 0 end }'      > $REPO/processing/pl/tablex.lua

# init.lua: absolute Pfade statt /processing/
sed "s|/processing/|$REPO/processing/|g" $REPO/processing/init.lua > /tmp/init_local.lua

# Lua-Datei patchen: require('init') → dofile mit absolutem Pfad
sed 's|require.*init.*|dofile("/tmp/init_local.lua")|' \
  $REPO/processing/topics/roads_bikelanes/roads_bikelanes.lua \
  > /tmp/roads_bikelanes_patched.lua
```

### Datenbank vorbereiten

```bash
psql -d osm -c "CREATE DATABASE osm_lua;"
psql -d osm_lua -c "CREATE EXTENSION postgis;"
psql -d osm_lua -c "CREATE EXTENSION btree_gist;"  # für (minzoom, geom) GiST-Index
```

### Pipeline ausführen

```bash
osm2pgsql \
  --create \
  --output=flex \
  --extra-attributes \
  --style=/tmp/roads_bikelanes_patched.lua \
  -H /var/run/postgresql \
  -d osm_lua \
  -U rush42 \
  /tmp/berlin-latest.osm.pbf
```

Berlin: ~71 Sekunden. Ergebnis in DB `osm_lua`: Tabellen `bikelanes`, `roads`, `bikelanesPresence`, `roadsPathClasses`, `bikeSuitability`, `todos_lines`.

---

## PostgreSQL

```bash
# Verbindung (Unix-Socket, kein Passwort)
psql -d osm -U rush42

# Wichtige Datenbanken
# osm       → Rust-Pipeline-Output (bikelanes + roads)
# osm_lua   → Lua-Pipeline-Output (alle 6 Tabellen)

# Kategorien vergleichen
psql -d osm     -c "SELECT derived->>'category', COUNT(*) FROM bikelanes GROUP BY 1 ORDER BY 2 DESC LIMIT 10;"
psql -d osm_lua -c "SELECT tags->>'category',    COUNT(*) FROM bikelanes GROUP BY 1 ORDER BY 2 DESC LIMIT 10;"
```
