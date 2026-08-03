// Minimal RFC 4180 CSV, both directions — used to talk to `psql`'s `COPY ... WITH (FORMAT csv)`
// and to the pipeline's own CSV output/input (`src/output/rows.rs`'s `write_csv_row`, `src/
// csv_source.rs`'s reader): quote a field containing a comma/quote/newline, double any quote inside
// it. No external CSV dependency in this package (see `package.json`), and the format is simple
// enough not to need one.

export function csvField(field: string): string {
  return /[",\n]/.test(field) ? `"${field.replace(/"/g, '""')}"` : field;
}

export function csvLine(fields: string[]): string {
  return fields.map(csvField).join(",") + "\n";
}

// Parses a full CSV text (not line-by-line — a quoted field can itself contain a newline) into
// rows of fields. No header handling; callers slice off row 0 themselves when the source writes one.
export function parseCsv(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let inQuotes = false;
  let i = 0;
  const n = text.length;
  while (i < n) {
    const c = text[i];
    if (inQuotes) {
      if (c === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i += 2;
          continue;
        }
        inQuotes = false;
        i++;
        continue;
      }
      field += c;
      i++;
      continue;
    }
    if (c === '"') {
      inQuotes = true;
      i++;
      continue;
    }
    if (c === ",") {
      row.push(field);
      field = "";
      i++;
      continue;
    }
    if (c === "\n" || c === "\r") {
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
      i += c === "\r" && text[i + 1] === "\n" ? 2 : 1;
      continue;
    }
    field += c;
    i++;
  }
  // Trailing field/row (a well-formed file ends in "\n", so this only fires for a missing final
  // newline — still worth keeping rather than silently dropping the last row).
  if (field !== "" || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  return rows.filter((r) => !(r.length === 1 && r[0] === ""));
}
