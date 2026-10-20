/* uapp browser build — saving the open document into this browser's
 * storage, saying when that fails, and what happens when the engine itself
 * has crashed. */

import { appName, errText, lastInfoName, leaving, markLeaving, mb } from "./env.js?v=__UAPP_BUILD__";
import { switchTo } from "./frame-router.js?v=__UAPP_BUILD__";
import { siteKey, siteState } from "./site-sync.js?v=__UAPP_BUILD__";
import { errorCard, hideBanner, showBanner } from "./splash.js?v=__UAPP_BUILD__";
import { appWrite, indexTouch, kvSet } from "./storage.js?v=__UAPP_BUILD__";
import { exportBytes, zombie } from "./worker-bridge.js?v=__UAPP_BUILD__";

// The stored document this page saves to: an app library id, or a site
// copy's id (siteState). null is the launcher, which is never saved.
export let currentAppId = null;
export function setCurrentApp(id) { currentAppId = id; }

// ---- auto-save ----------------------------------------------------------------
// A change schedules a save 1.5 s later, and further changes push it back,
// but never past 5 s after the first unsaved one: an AI run writes for
// minutes on end and would otherwise never be saved at all. One save runs at
// a time (savingPromise); closing, resetting and handing the document to
// another tab all wait for it rather than racing it.
const SAVE_DELAY = 1500, SAVE_MAX_WAIT = 5000;
let saveTimer = null;
let dirtySince = 0;       // ms of the first change no save has captured yet; 0 = clean
let savingPromise = null; // the save in flight, resolves to true when it landed
let saveAgain = false;    // a save was asked for while one was running
let lastSavedAt = 0;
let saveState = "saved";  // saved | dirty | saving | failed
let saveError = "";

// The launcher is ephemeral, dormant tabs must not write, and a crashed
// engine has nothing trustworthy left to save.
function canSave() { return !!currentAppId && !zombie && !engineDead; }

let engineDead = "";
export function onEngineDead(why) {
  if (engineDead) return;
  engineDead = String(why || "unknown error");
  clearTimeout(saveTimer); saveTimer = null;
  console.error("uapp: the engine crashed:", engineDead);
  let text = "The app's engine hit an internal error and stopped, so nothing more can be saved from this page. ";
  if (currentAppId) {
    text += lastSavedAt
      ? `Your last save was at ${new Date(lastSavedAt).toLocaleTimeString()}. Reload to carry on from there.`
      : "Nothing was saved since this page opened. Reload to carry on from the last saved copy.";
  } else {
    text += "Reload to start again.";
  }
  errorCard("Something went wrong", text + "\n\n" + engineDead, [["Reload", () => { markLeaving(); location.reload(); }]]);
}

function setSaveState(state, err) {
  saveState = state;
  saveError = err ? errText(err) : "";
  if (state === "failed") saveFailureBanner();
  else if (state === "saved") hideBanner("uapp-save-failed");
  try { window.dispatchEvent(new CustomEvent("uapp-save-state", { detail: window.__uappSaveState() })); } catch {}
}
window.__uappSaveState = () => ({ state: saveState, error: saveError, lastSavedAt, dirty: !!dirtySince });

export function scheduleSave() {
  if (!canSave()) return;
  const now = Date.now();
  if (!dirtySince) dirtySince = now;
  if (saveState !== "failed" && !savingPromise) setSaveState("dirty");
  clearTimeout(saveTimer);
  const wait = Math.max(0, Math.min(SAVE_DELAY, dirtySince + SAVE_MAX_WAIT - now));
  saveTimer = setTimeout(() => { saveTimer = null; appSave(); }, wait);
}

function appSave() {
  if (!canSave()) return Promise.resolve(false);
  if (savingPromise) { saveAgain = true; return savingPromise; }
  savingPromise = (async () => {
    let ok;
    do { saveAgain = false; ok = await saveOnce(); } while (ok && saveAgain && canSave());
    return ok;
  })().finally(() => { savingPromise = null; });
  return savingPromise;
}

async function saveOnce() {
  clearTimeout(saveTimer); saveTimer = null; // this save covers every change so far
  const since = dirtySince;
  dirtySince = 0;
  setSaveState("saving");
  try {
    const bytes = await exportBytes();
    // Went dormant, reset or stood down while exporting: write nothing.
    if (!canSave()) return false;
    await appWrite(currentAppId, bytes);
    lastSavedAt = Date.now();
    if (siteState) {
      siteState.hasLocal = true;
      siteState.localSavedAt = lastSavedAt;
      kvSet(siteKey("saved"), String(siteState.localSavedAt));
    } else {
      indexTouch(currentAppId, lastInfoName);
    }
    setSaveState(dirtySince ? "dirty" : "saved");
    afterSave();
    return true;
  } catch (e) {
    console.warn("uapp: auto-save failed:", e);
    if (!dirtySince) dirtySince = since || Date.now();
    setSaveState("failed", e);
    return false;
  }
}

// Write anything pending now and wait for it. True when nothing is left
// unsaved. The shell awaits this before a navigation that leaves the page
// (a link out of the editor); it also runs when the tab is hidden.
export async function flushSave() {
  if (!canSave()) return true;
  if (saveTimer || (dirtySince && !savingPromise)) await appSave();
  await settleSaves();
  return !dirtySince && saveState !== "failed";
}
window.__uappFlushSave = flushSave;

// Drop a scheduled save and anything not yet saved (a reset is about to
// throw the copy away), then wait for one already writing to land.
export async function abandonSaves() {
  clearTimeout(saveTimer); saveTimer = null;
  dirtySince = 0;
  await settleSaves();
}
// Wait for the save in flight, if any (and any it chains into).
export async function settleSaves() {
  while (savingPromise) await savingPromise.catch(() => {});
}

// pagehide cannot wait for a save, so the real work is done earlier: when the
// tab is hidden (the last moment a mobile browser reliably gives us), and by
// the prompt below while anything is unsaved.
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "hidden") flushSave(); });
window.addEventListener("pagehide", () => { flushSave(); });
window.addEventListener("beforeunload", (e) => {
  if (leaving || !canSave()) return;
  if (dirtySince || savingPromise || saveState === "failed") { e.preventDefault(); e.returnValue = ""; }
});

export function downloadName() { return (appName() || "app").replace(/[^\w \-.]/g, "_") + ".uapp"; }
async function downloadCopy() {
  const bytes = await exportBytes(true);
  const a = document.createElement("a");
  a.href = URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
  a.download = downloadName();
  document.body.appendChild(a);
  a.click();
  setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 30000);
}

// Once the visitor has real work here, ask the browser not to evict it under
// storage pressure (a silent clear-out is how an unpersisted origin loses
// everything), and say so when the origin is nearly out of room: the next
// failure would be a quota error.
let persistAsked = false, lastEstimate = 0, fullWarned = false;
async function afterSave() {
  const st = navigator.storage;
  if (!st) return;
  if (!persistAsked && st.persist) {
    persistAsked = true;
    try { if (!(st.persisted && await st.persisted())) await st.persist(); } catch {}
  }
  if (fullWarned || !st.estimate || Date.now() - lastEstimate < 60000) return;
  lastEstimate = Date.now();
  try {
    const { usage, quota } = await st.estimate();
    if (quota && usage / quota > 0.9) {
      fullWarned = true;
      showBanner("uapp-storage-full", "Storage for this site is nearly full",
        `${mb(usage)} of ${mb(quota)} used. Download a copy of this app so a failed save can't cost you work, and delete apps you no longer need.`,
        [["Download a copy", () => downloadCopy().catch((e) => alert("Could not download a copy: " + errText(e)))],
         ["Got it", (el) => el.remove()]]);
    }
  } catch {}
}

export function saveFailureBanner(closing) {
  const buttons = [
    ["Download a copy", () => downloadCopy().catch((e) => alert("Could not download a copy: " + errText(e)))],
    ["Try again", () => appSave()],
  ];
  if (closing) buttons.push(["Close anyway", () => { markLeaving(); switchTo(null); }]);
  showBanner("uapp-save-failed", "Your changes could not be saved in this browser",
    (saveError || "The save failed.") + (lastSavedAt ? ` Last saved at ${new Date(lastSavedAt).toLocaleTimeString()}.` : ""),
    buttons);
}
