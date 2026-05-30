# Pipeline-Übersicht: Lua & SQL Verarbeitungspipeline

## Zweck

Die Pipeline verarbeitet OpenStreetMap-Rohdaten (PBF-Format) zu sauberen, tile-fähigen PostgreSQL-Tabellen für ein Kartenprojekt mit Fokus auf Parken, Radinfrastruktur und städtische Infrastruktur. Der Kern liegt in `processing/`.

---

## Gesamtarchitektur

```
OSM-Daten (PBF)
    ↓
[osm2pgsql + Lua]   → Rohtabellen in PostgreSQL
    ↓
[SQL-Nachverarbeitung] → Aufbereitete, tile-fertige Tabellen
    ↓
[Diffing]            → Änderungstabellen (_diff) mit CHANGE-Metadaten
```

Der Einstiegspunkt ist `processing/index.ts`, die Topic-Schleife in `processing/steps/processTopics.ts`.

---

## Topics (Themengebiete)

Jedes Topic hat eine `.lua`-Datei (osm2pgsql-Import) und optional eine `.sql`-Datei (Nachverarbeitung).

| Topic | Lua-Datei | Inhalt |
|---|---|---|
| Parking | `topics/parking/parking.lua` | Straßenparken, Hindernisse, Querungen, ÖPNV-Halteverbote |
| Roads & Bikelanes | `topics/roads_bikelanes/roads_bikelanes.lua` | Straßen, Radwege, Oberfläche, Beleuchtung, Fahrradeignung |
| Land Use | `topics/landuse/landuse.lua` | Flächennutzung (Wohnen, Gewerbe, Schulen, …) |
| Barriers | `topics/barriers/barriers.lua` | Gewässer, Bahnlinien, Flughafengrenzen |
| Bike Routes | `topics/bikeroutes/bikeroutes.lua` | Radrouten-Relationen mit Distanz und Netzeinteilung |
| Traffic Signs | `topics/trafficSigns/trafficSigns.lua` | Verkehrszeichen-POIs mit Richtungsattributen |
| Places | `topics/places/places.lua` | Orte (Städte, Dörfer) mit Einwohnerzahl |
| Public Transport | `topics/publicTransport/publicTransport.lua` | Tram-, Bahn-, Fährhaltestellen |
| POI Classification | `topics/poiClassification/poiClassification.lua` | Einteilung von Shops und Einrichtungen in 4 Kategorien |
| Boundaries | `topics/boundaries/boundaries.lua` | Verwaltungsgrenzen |
| Bicycle Parking | `topics/bicycleParking/bicycleParking.lua` | Fahrradabstellanlagen |

---

## Lua-Schicht (osm2pgsql-Import)

### Aufgabe

Lua-Skripte laufen direkt in osm2pgsql und streamen OSM-Objekte (Nodes, Ways, Relations) in PostgreSQL-Tabellen. Sie sind **keine** eigenständigen Programme, sondern Callbacks des osm2pgsql-Frameworks.

### Wichtige Hilfsbibliotheken (`processing/topics/*/helper/`)

**Allgemein:**
- `DefaultId.lua` – Generiert eindeutige IDs im Format `osm_type/osm_id`
- `Metadata.lua` – Extrahiert OSM-Metadaten (`updated_at`, `updated_by`, `changeset_id`)
- `ExtractPublicTags.lua` – Filtert interne Tags (Präfix `_`) aus der Ausgabe
- `MergeTable.lua` / `Clone.lua` / `Set.lua` – Tabellenoperationen
- `CompareTables.lua` – String-basierter Tabellenvergleich (für Skip-Logik)
- `Sanitize.lua` / `sanitize_string.lua` / `sanitize_values.lua` – Bereinigung von Tag-Werten

**Bikelanes:**
- `Bikelanes.lua` – Hauptextraktion und Kategorisierung von Radwegen
- `BikelaneCategories.lua` – 20+ Kategorien (getrennt, geschützt, Schutzstreifen, …)
- `BikeLaneGeneralization.lua` – Bestimmt `minzoom` pro Kategorie
- `BikelanesPresence.lua` – Markiert Vorhandensein/Fehlen von Radwegen
- `BikeSuitability.lua` – Berechnet Fahrradeignungsscore
- `IsSidepath.lua` – Erkennt Fuß-/Radwege als Gehwegbegleiter

**Parking:**
- `parking_parkings.lua` – Hauptextraktion von Way-Parkierungen
- `classify_parking_conditions.lua` – Parst konditionelle Parkbeschränkungen
- `parse_conditional_value.lua` – Zeitbasierte Parsbedingungen (`Mo-Fr 8:00-18:00`)
- `invert_time_condition.lua` / `subtract_time_ranges.lua` – Zeitbereichslogik
- `capacity_tags.lua` / `capacity_normalization.lua` – Kapazitätsberechnung
- `transform_parkings.lua` – Verarbeitung von `left`/`right`/`both`-Seiten

**CSV-Integration:**
- `load_csv_mapillary_coverage.lua` – Lädt Mapillary-Abdeckungsdaten
- `load_csv_is_sidepath.lua` – Lädt Gehwegbegleiter-Klassifikation

### Tabellenstruktur (Pflichtfelder je Tabelle)

Jede osm2pgsql-Tabelle muss `id`, `tags`, `geom`, `meta`, `minzoom` enthalten — das prüft `utils/TableNames.lua` beim Einlesen der Tabellennamen.

---

## SQL-Schicht (Nachverarbeitung)

### Parking-Pipeline (`topics/parking/parking.sql`)

Die umfangreichste SQL-Pipeline mit ~30 Dateien, hierarchisch geordnet:

**1. Hilfsfunktionen:**
- `project_to_k_closest_kerbs.sql` – Projektion eines Punktes auf die k nächsten Bordsteinkanten
- `project_to_closest_platform.sql` – Projektion auf ÖPNV-Bahnsteige
- `estimate_capacity.sql` – Schätzt Parkkapazität aus Geometrielänge
- `parking_area_to_line.sql` – Konvertiert Flächen-Parkierungen zu Linien

**2. Bordstein-Netz (Kerbs):**
- `0_create_kerbs.sql` – Extrahiert Straßenmittelachsen, erstellt Bordsteinkanten
- `1_find_intersections.sql` – Identifiziert Kreuzungspunkte
- `2_find_intersection_corners.sql` – Berechnet Eckpunkte an Kreuzungen
- `3_find_driveways.sql` – Erkennt Einfahrten
- `5_trim_kerbs.sql` – Kürzt Bordsteine an Kreuzungen
- `6_driveway_corners_kerbs.sql` – Einfahrt-Ecken-Interaktionen

**3. Geometrie-Projektion (Querungen, Hindernisse, ÖPNV, separate Parkierungen):**
Für jede Kategorie: Punkte/Linien/Flächen werden auf das Bordstein-Netz projiziert.

**4. Cutouts (Ausschnitte):**
- Kreuzungen, Einfahrten und Hindernisse erzeugen `cutout`-Geometrien
- `1_cutout_road_parkings.sql` / `2_cutout_separate_parkings.sql` – Wendet `ST_Difference` an
- Externe Cutouts aus EUVM-Daten (`2_external_cutouts_euvm.sql`)

**5. Finalisierung:**
- `3_redistribute_parking_capacities.sql` – Verteilt Kapazitäten nach Geometrieschnitt neu
- `4_merge_parkings.sql` – Führt benachbarte Parkierungen mit gleichen Tags zusammen
- `5_estimate_parking_capacities.sql` – Schätzt fehlende Kapazitäten
- `6_filter_parkings.sql` – Filtert ungültige Einträge
- `7_finalize_parkings.sql` – Formatierung und Abschluss
- `8_create_quantized_tables.sql` – Quantisierung für Tile-Generierung
- `10_create_labels.sql` – Erzeugt Parkierungs-Beschriftungen

### Roads & Bikelanes-Pipeline (`topics/roads_bikelanes/roads_bikelanes.sql`)

Schlankere Nachverarbeitung:
- Verschiebt Bikelanes aus Straßentabellen falls nötig
- Bereinigt TODO-Linien

---

## Diffing-System

### Zweck

Erkennt welche Features sich zwischen zwei Pipeline-Läufen geändert, hinzugefügt oder entfernt wurden. Ausgabe sind `_diff`-Tabellen mit identischem Schema wie die Originaltabelle plus einem `CHANGE`-Feld.

### Dateien

| Datei | Inhalt |
|---|---|
| `diffing/diffing.ts` | Hauptlogik: Referenztabellen erstellen, Diff berechnen |
| `diffing/jsonb_diff.sql` | PostgreSQL-Funktion `jsonb_diff(old, new)` |

### SQL-Funktion `jsonb_diff`

```sql
jsonb_diff(old JSONB, new JSONB) RETURNS JSONB
```

Vergleicht zwei JSONB-Objekte Feld für Feld:
- Änderung: `"key": "alter_wert -> neuer_wert"`
- Löschung: `"key": "(-) alter_wert"`
- Hinzufügung: `"key": "(+) neuer_wert"`

### Drei Diffing-Modi

| Modus | Verhalten |
|---|---|
| `reference` | Erstellt neue Baseline, löscht alle vorherigen Diffs |
| `previous` | Vergleicht aktuellen Lauf mit vorherigem, erstellt inkrementell neue Referenzen |
| `fixed` | Eingefroren: Referenztabellen werden nur für neue Tabellen erstellt |

### Diffing-Algorithmus

```
1. Referenztabellen erstellen (nach PROCESSING_DIFFING_BBOX gefiltert)
2. Topic-Pipeline ausführen (osm2pgsql + SQL)
3. Full Outer Join: reference LEFT JOIN current ON id
4. Kategorisierung:
   - MODIFIED: old_id = new_id UND old_tags ≠ new_tags
   - ADDED:    old_id IS NULL (nur im aktuellen Lauf)
   - REMOVED:  new_id IS NULL (nur in der Referenz)
5. Eintrag in table_diff mit CHANGE-Tag
```

### Skip-Bedingungen

Der Diff wird übersprungen wenn:
- Neue OSM-Daten heruntergeladen wurden (kein stabiler Vergleichspunkt)
- Hilfsdateien oder Konstanten geändert wurden
- Modus ist `reference`

### Räumliche Einschränkung

Referenztabellen werden auf `PROCESSING_DIFFING_BBOX` zugeschnitten. Bei gesetztem `PROCESS_ONLY_BBOX` wird der Schnitt beider Bounding Boxes verwendet.

---

## Tabellennamen-Extraktion

`utils/TableNames.lua` simuliert die osm2pgsql-Bibliothek und extrahiert Tabellennamen aus Lua-Dateien ohne sie auszuführen. Das ermöglicht dem TypeScript-Orchestrator zu wissen, welche Tabellen ein Topic erzeugt — ohne einen echten Import-Lauf.

Tabellen mit `todos_lines` oder `parking_errors` im Namen werden beim Diffing ignoriert.

---

## Dateistruktur (Pipeline-relevant)

```
processing/
├── index.ts                     # Einstiegspunkt
├── steps/processTopics.ts       # Topic-Schleife mit Diffing
├── diffing/
│   ├── diffing.ts               # Diffing-Logik
│   └── jsonb_diff.sql           # SQL-Vergleichsfunktion
├── utils/
│   └── TableNames.lua           # Tabellennamen-Extraktion
├── init.lua                     # Auto-generierte Lua-Paketpfade
├── topics/
│   ├── parking/
│   │   ├── parking.lua          # osm2pgsql-Import
│   │   ├── parking.sql          # SQL-Orchestrator (~30 Dateien)
│   │   └── helper/              # 30+ Lua-Hilfsbibliotheken
│   ├── roads_bikelanes/
│   │   ├── roads_bikelanes.lua
│   │   ├── roads_bikelanes.sql
│   │   └── helper/              # ~40 Lua-Hilfsbibliotheken
│   └── [weitere Topics]/
└── constants/
    ├── topics.const.ts          # Topic-Liste + optionale Bbox-Filter
    └── directories.const.ts     # Pfadkonstanten
```
