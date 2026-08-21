/* Result-grid export — CSV / TSV / minimal XLSX, plus the download helper.
 * No dependencies: the XLSX is a STORED (uncompressed) zip written by hand,
 * just the five parts a spreadsheet reader needs. Values are normalized the
 * same way everywhere: NULL → empty, blobs → "[blob]", numbers stay numbers. */

/// Normalize one exported value (shared by CSV / TSV / XLSX).
const plain = (v) =>
  v === null || v === undefined ? ""
  : typeof v === "object" && v.b64 ? "[blob]"
  : v;

// ---- CSV (RFC 4180: quote when needed, "" for ", CRLF rows) ----
export function toCsv(cols, rows) {
  const field = (v) => {
    const s = String(plain(v));
    return /[",\r\n]/.test(s) ? '"' + s.replace(/"/g, '""') + '"' : s;
  };
  const line = (r) => r.map(field).join(",");
  return [cols, ...rows].map(line).join("\r\n") + "\r\n";
}

// ---- TSV (tabs/newlines inside a value become a space) ----
export function toTsv(cols, rows) {
  const field = (v) => String(plain(v)).replace(/\r\n|[\t\r\n]/g, " ");
  const line = (r) => r.map(field).join("\t");
  return [cols, ...rows].map(line).join("\n") + "\n";
}

// ---- XLSX ----
/// XML-escape, dropping the control chars XML 1.0 cannot carry at all.
const xesc = (s) => String(s)
  .replace(/[\x00-\x08\x0B\x0C\x0E-\x1F]/g, "")
  .replace(/&/g, "&amp;").replace(/</g, "&lt;")
  .replace(/>/g, "&gt;").replace(/"/g, "&quot;");
/// 0 → "A", 25 → "Z", 26 → "AA" …
const colRef = (n) => {
  let s = "";
  for (n++; n > 0; n = Math.floor((n - 1) / 26)) s = String.fromCharCode(65 + ((n - 1) % 26)) + s;
  return s;
};
function sheetXml(cols, rows) {
  const cell = (v, c, r) => {
    const p = plain(v);
    const ref = colRef(c) + (r + 1);
    if (typeof p === "number" && Number.isFinite(p)) return `<c r="${ref}"><v>${p}</v></c>`;
    if (p === "") return "";
    return `<c r="${ref}" t="inlineStr"><is><t xml:space="preserve">${xesc(p)}</t></is></c>`;
  };
  const rowXml = (vals, r) => `<row r="${r + 1}">` + vals.map((v, c) => cell(v, c, r)).join("") + `</row>`;
  return `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>` +
    `<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>` +
    [cols, ...rows].map(rowXml).join("") +
    `</sheetData></worksheet>`;
}
export function toXlsx(cols, rows, sheetName) {
  // Sheet names cannot hold []:*?/\ and cap at 31 chars.
  const name = (String(sheetName || "Sheet1").replace(/[[\]:*?/\\]/g, "_").slice(0, 31)) || "Sheet1";
  const XML = `<?xml version="1.0" encoding="UTF-8" standalone="yes"?>`;
  return zipStored([
    ["[Content_Types].xml", XML +
      `<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">` +
      `<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>` +
      `<Default Extension="xml" ContentType="application/xml"/>` +
      `<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>` +
      `<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>` +
      `</Types>`],
    ["_rels/.rels", XML +
      `<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">` +
      `<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>` +
      `</Relationships>`],
    ["xl/workbook.xml", XML +
      `<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"` +
      ` xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">` +
      `<sheets><sheet name="${xesc(name)}" sheetId="1" r:id="rId1"/></sheets></workbook>`],
    ["xl/_rels/workbook.xml.rels", XML +
      `<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">` +
      `<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>` +
      `</Relationships>`],
    ["xl/worksheets/sheet1.xml", sheetXml(cols, rows)],
  ]);
}

// ---- a tiny STORED zip writer ----
let crcTable = null;
function crc32(bytes) {
  if (!crcTable) {
    crcTable = new Int32Array(256);
    for (let n = 0; n < 256; n++) {
      let c = n;
      for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
      crcTable[n] = c;
    }
  }
  let c = -1;
  for (let i = 0; i < bytes.length; i++) c = crcTable[(c ^ bytes[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}
const DOS_DATE = ((2026 - 1980) << 9) | (1 << 5) | 1; // 2026-01-01, zips want *a* date
/// entries: [name, text][] → the zip bytes (local headers, central dir, EOCD).
function zipStored(entries) {
  const enc = new TextEncoder();
  const chunks = [], central = [];
  let offset = 0;
  for (const [name, text] of entries) {
    const nameB = enc.encode(name), data = enc.encode(text);
    const crc = crc32(data);
    const local = new Uint8Array(30 + nameB.length);
    const lv = new DataView(local.buffer);
    lv.setUint32(0, 0x04034b50, true);  // local file header
    lv.setUint16(4, 20, true);          // version needed
    lv.setUint16(6, 0x0800, true);      // flags: UTF-8 names
    lv.setUint16(8, 0, true);           // method: STORED
    lv.setUint16(10, 0, true);          // mod time
    lv.setUint16(12, DOS_DATE, true);   // mod date
    lv.setUint32(14, crc, true);
    lv.setUint32(18, data.length, true); // compressed = raw (stored)
    lv.setUint32(22, data.length, true);
    lv.setUint16(26, nameB.length, true);
    lv.setUint16(28, 0, true);          // extra length
    local.set(nameB, 30);
    chunks.push(local, data);
    const cen = new Uint8Array(46 + nameB.length);
    const cv = new DataView(cen.buffer);
    cv.setUint32(0, 0x02014b50, true);  // central directory header
    cv.setUint16(4, 20, true);          // version made by
    cv.setUint16(6, 20, true);          // version needed
    cv.setUint16(8, 0x0800, true);
    cv.setUint16(10, 0, true);
    cv.setUint16(12, 0, true);
    cv.setUint16(14, DOS_DATE, true);
    cv.setUint32(16, crc, true);
    cv.setUint32(20, data.length, true);
    cv.setUint32(24, data.length, true);
    cv.setUint16(28, nameB.length, true);
    cv.setUint32(42, offset, true);     // local header offset
    cen.set(nameB, 46);
    central.push(cen);
    offset += local.length + data.length;
  }
  const cenSize = central.reduce((s, c) => s + c.length, 0);
  const eocd = new Uint8Array(22);
  const ev = new DataView(eocd.buffer);
  ev.setUint32(0, 0x06054b50, true);    // end of central directory
  ev.setUint16(8, entries.length, true);
  ev.setUint16(10, entries.length, true);
  ev.setUint32(12, cenSize, true);
  ev.setUint32(16, offset, true);
  const out = new Uint8Array(offset + cenSize + 22);
  let p = 0;
  for (const c of [...chunks, ...central, eocd]) { out.set(c, p); p += c.length; }
  return out;
}

// ---- download ----
export function downloadBlob(filename, mime, data) {
  // Android's DownloadManager can't fetch blob: URLs at all — hand the bytes
  // to the native bridge, which writes them into the system Downloads folder.
  // (Older APKs without saveBlob fall through to the anchor click.)
  if (typeof window.UAppAndroid?.saveBlob === "function") {
    const bytes = typeof data === "string" ? new TextEncoder().encode(data)
      : data instanceof Uint8Array ? data : new Uint8Array(data);
    let bin = "";
    for (let i = 0; i < bytes.length; i += 0x8000) {
      bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    }
    window.UAppAndroid.saveBlob(filename, mime || "application/octet-stream", btoa(bin));
    return;
  }
  const url = URL.createObjectURL(new Blob([data], { type: mime }));
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
