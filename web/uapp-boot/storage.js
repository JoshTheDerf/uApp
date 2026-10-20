/* uapp browser build — this browser's app library: the index (localStorage,
 * with an in-memory fallback) and the .uapp bytes (OPFS, IndexedDB, or
 * nothing at all when the browser blocks site data). */

import { errText } from "./env.js?v=__UAPP_BUILD__";
import { showBanner } from "./splash.js?v=__UAPP_BUILD__";

const APPS_DIR = "apps";
const LEGACY_FILE = "app.uapp";

// localStorage *throws* (rather than returning null) when the browser blocks
// site data for the origin, so everything goes through a shim that keeps the
// index in memory for the life of the tab instead.
const memKV = new Map();
export function kvGet(key) {
  if (memKV.has(key)) return memKV.get(key); // a write that localStorage refused (quota) lives here
  try { return localStorage.getItem(key); } catch { return null; }
}
export function kvSet(key, val) {
  try { localStorage.setItem(key, val); } catch { memKV.set(key, val); }
}
export function kvDel(key) { try { localStorage.removeItem(key); } catch {} memKV.delete(key); }

export function appsIndex() {
  try { return JSON.parse(kvGet("uapp.demo.apps")) || []; } catch { return []; }
}
function saveIndex(list) {
  kvSet("uapp.demo.apps", JSON.stringify(list));
}
export function indexTouch(id, name) {
  const list = appsIndex();
  const e = list.find((a) => a.id === id);
  if (e) { if (name) e.name = name; e.updated = Date.now(); }
  else list.push({ id, name: name || "App", updated: Date.now() });
  saveIndex(list);
}

async function opfsDir() {
  const root = await navigator.storage.getDirectory();
  return root.getDirectoryHandle(APPS_DIR, { create: true });
}

const opfsStore = {
  name: "OPFS",
  persistent: true,
  async read(id) {
    const f = await (await (await opfsDir()).getFileHandle(id + ".uapp")).getFile();
    return f.size > 0 ? new Uint8Array(await f.arrayBuffer()) : null;
  },
  async write(id, bytes) {
    const fh = await (await opfsDir()).getFileHandle(id + ".uapp", { create: true });
    const w = await fh.createWritable();
    await w.write(bytes);
    await w.close();
  },
  async remove(id) { await (await opfsDir()).removeEntry(id + ".uapp"); },
};

const IDB_NAME = "uapp.demo", IDB_STORE = "apps";
function idbOpen() {
  return new Promise((resolve, reject) => {
    let req;
    try { req = indexedDB.open(IDB_NAME, 1); } catch (e) { reject(e); return; }
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(IDB_STORE)) db.createObjectStore(IDB_STORE);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error || new Error("indexedDB.open failed"));
    req.onblocked = () => reject(new Error("indexedDB.open blocked"));
  });
}
async function idbRun(mode, fn) {
  const db = await idbOpen();
  try {
    return await new Promise((resolve, reject) => {
      const tx = db.transaction(IDB_STORE, mode);
      const req = fn(tx.objectStore(IDB_STORE));
      tx.oncomplete = () => resolve(req ? req.result : undefined);
      tx.onabort = () => reject(tx.error || new Error("transaction aborted"));
      if (req) req.onerror = () => reject(req.error);
    });
  } finally { try { db.close(); } catch {} }
}

const idbStore = {
  name: "IndexedDB",
  persistent: true,
  async read(id) {
    const v = await idbRun("readonly", (s) => s.get(id));
    if (!v) return null;
    const u8 = v instanceof Uint8Array ? v : new Uint8Array(v);
    return u8.length > 0 ? u8 : null;
  },
  write(id, bytes) { return idbRun("readwrite", (s) => s.put(bytes.slice(), id)); },
  remove(id) { return idbRun("readwrite", (s) => s.delete(id)); },
};

// Nothing left to write to. Reads come back empty and writes say why, which
// beats "opening" an app whose bytes silently vanish on the next reload (every
// document switch here is a reload).
const BLOCKED_MSG =
  "this browser is blocking storage for the site — a private window, or " +
  "cookies/site data blocked for it. Allow site data (or use a normal window) " +
  "to open apps in the demo.";
const blockedStore = {
  name: "none",
  persistent: false,
  async read() { return null; },
  async write() { throw new Error(BLOCKED_MSG); },
  async remove() {},
};

let blobStoreP = null;
export function blobStore() { return (blobStoreP = blobStoreP || pickBlobStore()); }

async function pickBlobStore() {
  try {
    // Probe a real write: a handle can be granted and the write still refused.
    const dir = await opfsDir();
    const fh = await dir.getFileHandle(".probe", { create: true });
    const w = await fh.createWritable();
    await w.write(new Uint8Array([0]));
    await w.close();
    try { await dir.removeEntry(".probe"); } catch {}
    return opfsStore;
  } catch (e) {
    console.warn("uapp: OPFS unavailable (" + errText(e) + ") — falling back to IndexedDB");
  }
  try {
    (await idbOpen()).close();
    return idbStore;
  } catch (e) {
    console.warn("uapp: IndexedDB unavailable (" + errText(e) + ") — apps will not be saved");
  }
  storageNotice();
  return blockedStore;
}

// Shown once when nothing can be persisted — the app-open failures below are
// otherwise a bare error message with no way to act on it.
let noticeShown = false;
function storageNotice() {
  if (noticeShown) return;
  noticeShown = true;
  showBanner("uapp-storage-blocked", "This browser is blocking storage for the site",
    "Usually a private window, or cookies and site data blocked for it. The demo " +
    "keeps a document per app in your browser's own storage, so opening one " +
    "can't work here. Allow site data for this site, or try a normal window.",
    [["Got it", (el) => el.remove()]]);
}

// null = no stored app with that id. Any OTHER failure (a locked store, a
// private-window quirk) is thrown: callers must not mistake it for "new app"
// and save an empty document over the real one.
export async function appRead(id) {
  try {
    const bytes = await (await blobStore()).read(id);
    return bytes || null;
  } catch (e) {
    if (e && e.name === "NotFoundError") return null;
    throw new Error("could not read the stored app: " + ((e && e.message) || e));
  }
}
export async function appWrite(id, bytes) {
  return (await blobStore()).write(id, bytes);
}
export async function appDelete(id) {
  try { await (await blobStore()).remove(id); } catch {}
  saveIndex(appsIndex().filter((a) => a.id !== id));
}
export const genId = () => "a" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);

// One-time migration of the single-app storage from earlier demo builds.
export async function migrateLegacy() {
  try {
    const root = await navigator.storage.getDirectory();
    const fh = await root.getFileHandle(LEGACY_FILE);
    const f = await fh.getFile();
    if (f.size > 0) {
      const id = genId();
      await appWrite(id, new Uint8Array(await f.arrayBuffer()));
      indexTouch(id, kvGet("uapp.demo.name") || "App");
    }
    await root.removeEntry(LEGACY_FILE);
  } catch {}
}
