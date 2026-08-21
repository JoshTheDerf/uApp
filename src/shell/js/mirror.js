/* Android write-back — keeps a linked original .uapp file current.
 *
 * On Android an intent-opened document is imported into a private working
 * copy (SAF gives no durable path to run SQLite on). Once the user re-picks
 * the original through the system picker (Settings → Original file), the
 * native side holds a persistable read/write grant; this module watches for
 * writes and mirrors the app back to it — always via /download.uapp, which
 * is a consistent backup-API export, never raw bytes of the live db.
 *
 * Kotlin calls window.__uappMirrorFlush() from onPause so the copy is
 * current whenever the user leaves the app. */

import { on } from "./core.js";
import { bytesToB64 } from "./ui.js";

let appPath = null;
let linked = false;
let unsavedDoc = false;
let timer = null;
let flushing = false;
let dirtyWhileFlushing = false;

const bridge = () => window.UAppAndroid;

/// Does this APK carry the write-back bridge at all?
export function mirrorSupported() {
  return typeof bridge()?.writeLinked === "function";
}

/// Re-read the native side's link state (after linking/unlinking, and once
/// the app path is known). Returns the new state. Broadcasts a "uapp-mirror"
/// window event so the topbar's "Not linked" warning stays accurate:
/// `relevant` is true only for intent-opened documents (an unsaved scratch
/// app has its own "Not saved" banner instead).
export function mirrorRefresh(path) {
  appPath = path || appPath;
  linked = !!(mirrorSupported() && appPath && bridge().mirrorStatus(appPath) === "linked");
  window.dispatchEvent(new CustomEvent("uapp-mirror", {
    detail: { linked, relevant: mirrorSupported() && !!appPath && !unsavedDoc },
  }));
  return linked;
}

async function flush() {
  clearTimeout(timer);
  timer = null;
  if (!linked || !appPath) return;
  if (flushing) {
    dirtyWhileFlushing = true;
    return;
  }
  flushing = true;
  try {
    const r = await fetch("/download.uapp");
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const bytes = new Uint8Array(await r.arrayBuffer());
    bridge().writeLinked(appPath, bytesToB64(bytes));
  } catch (e) {
    console.warn("uapp: write-back to the original file failed:", e);
  } finally {
    flushing = false;
    if (dirtyWhileFlushing) {
      dirtyWhileFlushing = false;
      schedule();
    }
  }
}

function schedule() {
  if (!linked) return;
  clearTimeout(timer);
  timer = setTimeout(flush, 3000);
}

on("info", (info) => {
  if (info.path && mirrorSupported()) {
    unsavedDoc = !!info.unsaved;
    mirrorRefresh(info.path); // cheap bridge call; keeps the warning fresh
  }
});
on("changes", schedule);

window.__uappMirrorFlush = () => {
  flush();
};
