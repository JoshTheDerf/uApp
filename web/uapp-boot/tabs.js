/* uapp browser build — one shell per stored document: the Web Lock and
 * BroadcastChannel handover between tabs (and frames) that open the same
 * one, the dormant state of the shell that lost it, and the hosted-site
 * chrome standing the engine down. */

import { flushSave, settleSaves } from "./autosave.js?v=__UAPP_BUILD__";
import { TAB_KEY, errText } from "./env.js?v=__UAPP_BUILD__";
import { markZombie, stopWorker, zombie } from "./worker-bridge.js?v=__UAPP_BUILD__";

// Hosted-site chrome that gave up (site-chrome.js abandon): stop the engine
// so the service worker no longer answers the plain page's requests from the
// in-browser copy, and stop saving.
window.__uappStandDown = () => {
  if (zombie) return;
  stopWorker("the editor was closed");
  if (releaseDocLock) { releaseDocLock(); releaseDocLock = null; }
};

// ---- one window per document -----------------------------------------------
// Two shells with the SAME stored document open would both auto-save it and
// clobber each other (one process owns the file, like the desktop app). So a
// boot announces what it opens, and an older shell on the same document
// hands it over and goes dormant. Different documents coexist — a desktop
// with several apps in several frames — since the service worker routes each
// shell's fetches by instance (see INST) and never needs a single answerer.
// The ephemeral launcher is never saved, so it is not guarded at all.
//
// The handover is a Web Lock per document, held for the shell's lifetime:
// the new shell asks for it and reads the stored copy only once it has it,
// and the old one releases it only after its last save has landed (so the
// new shell never opens a copy that is about to be overwritten). An old
// shell that does not let go in time (frozen in a background tab, or a build
// from before the lock) has it stolen, which puts it to sleep as well.
// Without Web Locks it is the BroadcastChannel announcement alone.

const BOOT_ID = Date.now() + "-" + Math.random().toString(36).slice(2);
const LOCK_STEAL_MS = 5000;
// Constructing it throws where the browser blocks site data for the origin,
// and this is module top level: an unhandled throw here leaves the page on the
// splash forever, so the takeover check simply goes away instead.
let tabChannel = null;
try {
  if ("BroadcastChannel" in window) tabChannel = new BroadcastChannel("uapp-demo-tab");
} catch (e) {
  console.warn("uapp: BroadcastChannel unavailable (" + errText(e) + ")");
}

let releaseDocLock = null; // resolves the promise our held lock waits on
// Say we are opening our document (an older shell on it hands over) and ask
// for its lock. Resolves once this shell owns it (or there is nothing to
// own, or no Web Locks to own it with). Never rejects: a lock failure must
// not keep the app from opening.
export function claimDocument() {
  if (tabChannel && TAB_KEY) tabChannel.postMessage({ takeover: true, bootId: BOOT_ID, key: TAB_KEY });
  return acquireDocLock();
}
function acquireDocLock() {
  const locks = navigator.locks;
  if (!TAB_KEY || !locks || !locks.request) return Promise.resolve();
  const name = "uapp-doc:" + TAB_KEY;
  return new Promise((resolve) => {
    let granted = false;
    const hold = () => {
      granted = true;
      resolve();
      return new Promise((r) => { releaseDocLock = r; });
    };
    // Rejected while held means another shell stole it: that shell is
    // reading the copy now, so stop at once, without a last save.
    const lost = (e) => {
      if (granted) goDormant(false);
      else { console.warn("uapp: document lock unavailable (" + errText(e) + ")"); resolve(); }
    };
    try {
      const ctl = new AbortController();
      const steal = setTimeout(() => {
        ctl.abort();
        locks.request(name, { steal: true }, hold).catch(lost);
      }, LOCK_STEAL_MS);
      locks.request(name, { signal: ctl.signal }, () => { clearTimeout(steal); return hold(); })
        .catch((e) => { if (!ctl.signal.aborted) { clearTimeout(steal); lost(e); } });
    } catch (e) { lost(e); }
  });
}

let dormant = null;
// `handOver`: another shell asked politely, so save first and then let go.
function goDormant(handOver) {
  if (dormant) return dormant;
  dormant = (async () => {
    if (handOver) { try { await flushSave(); } catch {} }
    markZombie(); // no save starts after this
    if (handOver) await settleSaves();
    stopWorker("this tab went dormant (the demo moved to another tab)");
    if (releaseDocLock) { releaseDocLock(); releaseDocLock = null; }
    showDormant();
  })();
  return dormant;
}

function showDormant() {
  const div = document.createElement("div");
  div.style.cssText = "position:fixed;inset:0;z-index:999999;display:flex;align-items:center;justify-content:center;background:#23293acc;backdrop-filter:blur(3px);color:#f2f4f8;font:15px/1.5 system-ui,sans-serif";
  div.innerHTML = `<div style="background:#2b3145;border:1px solid #3a4260;border-radius:14px;padding:26px;max-width:360px;text-align:center">
    <div style="font-size:30px">👋</div>
    <h1 style="font-size:17px;margin:8px 0 4px">Demo opened in another tab</h1>
    <p style="color:#aeb6c8;font-size:13px;margin:0 0 16px">The uapp demo runs in one tab at a time (one process owns the file, like the desktop app).</p>
    <button style="padding:10px 18px;border:none;border-radius:9px;background:#3f6de6;color:#fff;font:inherit;font-weight:600;cursor:pointer">Use it here instead</button>
  </div>`;
  div.querySelector("button").addEventListener("click", () => location.reload());
  document.body.appendChild(div);
}

if (tabChannel) {
  tabChannel.onmessage = (ev) => {
    const d = ev.data;
    if (d && d.takeover && d.bootId !== BOOT_ID && TAB_KEY && d.key === TAB_KEY) goDormant(true);
  };
}
