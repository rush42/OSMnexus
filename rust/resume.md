# Rust Bikelane Pipeline — Struktur

Reimplementierung der `roads_bikelanes` Lua/osm2pgsql-Pipeline in Rust.
Ersetzt: `processing/topics/roads_bikelanes/` (Lua + SQL).
Schreibt in dieselben PostgreSQL-Tabellen (`bikelanes`, `roads`).

## Ausführung

```bash
PGDATABASE=osm PGUSER=rush42 ./target/release/osm-bikelanes /tmp/berlin-latest.osm.pbf
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
