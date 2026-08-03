// Parses Postgres's `COPY ... TO STDOUT (FORMAT binary)` output (the file-level encoding; see the
// "Binary Format" section of the Postgres COPY docs — psql already unwraps the copy protocol
// framing before writing these bytes to its own stdout, same as it does for `FORMAT csv`). No type
// interpretation happens here — callers decode each returned field `Buffer` with whatever's
// appropriate for that column (`buf.readBigInt64BE()` for `int8`, `buf.toString("utf8")` for
// `text`, `linestringFromEwkb` for a `bytea` EWKB column, ...). Used by `fetchWays` in
// `lib/liveEditor.ts` to avoid `FORMAT csv`'s quoting/escaping and ASCII-text geometry cost.
const SIGNATURE = Buffer.from("PGCOPY\n\xff\r\n\0", "binary");

export function parseCopyBinary(buf: Buffer): (Buffer | null)[][] {
  if (buf.length < 19 || !buf.subarray(0, 11).equals(SIGNATURE)) {
    throw new Error("not a Postgres binary COPY stream (bad signature)");
  }
  let offset = 11;
  offset += 4; // flags field, unused (the OID-inclusion bit is long-deprecated)
  const extLen = buf.readInt32BE(offset);
  offset += 4 + extLen;

  const rows: (Buffer | null)[][] = [];
  while (offset < buf.length) {
    const fieldCount = buf.readInt16BE(offset);
    offset += 2;
    if (fieldCount === -1) break; // file trailer
    const row: (Buffer | null)[] = [];
    for (let i = 0; i < fieldCount; i++) {
      const len = buf.readInt32BE(offset);
      offset += 4;
      if (len === -1) {
        row.push(null);
      } else {
        row.push(buf.subarray(offset, offset + len));
        offset += len;
      }
    }
    rows.push(row);
  }
  return rows;
}
