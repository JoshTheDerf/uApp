/* uapp browser build — an archive that came from a URL (a hosted site's own
 * /site.uapp, ?open=, or the page's --open default): opening it (this
 * browser's saved copy, or a fresh download), resetting it to the server's,
 * and publishing it back. sync-pill.js is the UI for the last two. */

import { abandonSaves, setCurrentApp } from "./autosave.js?v=__UAPP_BUILD__";
import { OPEN, markLeaving } from "./env.js?v=__UAPP_BUILD__";
import { splashAsk, splashProgress, splashShow } from "./splash.js?v=__UAPP_BUILD__";
import { appRead, blobStore, kvDel, kvGet, kvSet } from "./storage.js?v=__UAPP_BUILD__";
import { exportBytes, markZombie, openApp } from "./worker-bridge.js?v=__UAPP_BUILD__";

// Hosted-site mode (site-chrome.js): the visitor's edits to the site are kept
// in this browser across reloads, under ONE stable id per origin — not in the
// launcher index (that is the demo's app library, a different thing on the
// same origin). `etag` is the /site.uapp ETag of the copy the edits were made
// on; the chrome compares it with the server's current one. Reset = drop the
// saved copy and reload from the server.
export let siteState = null; // {id, etag, hasLocal, localSavedAt}
export const siteKey = (k) => "uapp.site." + k + ":" + siteState.id;
// One stored copy per archive URL. A hosted site's own /site.uapp keeps the
// key it always had (per host), so nobody's pending edits vanish.
function siteIdFor(archive) {
  const u = new URL(archive, location.href);
  const clean = (s) => s.replace(/[^a-z0-9.-]/gi, "_");
  if (u.origin === location.origin && u.pathname === "/site.uapp") return "site-" + clean(location.host);
  return "site-" + clean(u.host + u.pathname);
}
// "…/uapp/apps/kanban-board.uapp" -> "kanban-board": the name to open an
// archive under when nothing better is known.
function archiveStem(archive) {
  try {
    const last = decodeURIComponent(new URL(archive, location.href).pathname.split("/").filter(Boolean).pop() || "");
    return last.replace(/\.uapp$/i, "").slice(0, 80) || "Site";
  } catch { return "Site"; }
}

// Open the archive at `site`: this browser's copy when there is one, else a
// fresh download (after asking, for one from another site). Resolves to
// app.info, or null when the visitor said no.
export async function openSiteArchive(site) {
  const siteName = window.__uappSiteName || archiveStem(site);
  siteState = { id: siteIdFor(site), etag: "", hasLocal: false, localSavedAt: 0 };
  let info;
  const saved = await appRead(siteState.id);
  if (saved) {
    siteState.hasLocal = true;
    siteState.localSavedAt = Number(kvGet(siteKey("saved"))) || 0;
    siteState.etag = kvGet(siteKey("etag")) || "";
    info = await openApp(saved, siteName);
  } else {
    if (!(await allowForeignOpen(site))) return null;
    splashShow("Opening " + siteName + "…", "Downloading…");
    const r = await fetch(new URL(site, location.href), { cache: "no-store" });
    if (!r.ok) throw new Error(`could not fetch ${siteName} (HTTP ${r.status})`);
    siteState.etag = r.headers.get("etag") || "";
    kvSet(siteKey("etag"), siteState.etag);
    splashProgress(0, Number(r.headers.get("content-length")) || 0);
    info = await openApp(new Uint8Array(await r.arrayBuffer()), siteName);
  }
  // Saved from the first change on (scheduleSave fires on "changes").
  setCurrentApp(siteState.id);
  window.__uappSiteState = siteState;
  return info;
}

window.__uappSiteReset = async () => {
  markLeaving();
  if (!siteState) { location.reload(); return; }
  markZombie(); // no new save may start...
  // ...and one already writing must land BEFORE the copy is removed, or it
  // would put the copy straight back.
  await abandonSaves();
  try { await (await blobStore()).remove(siteState.id); } catch {}
  kvDel(siteKey("saved"));
  kvDel(siteKey("etag"));
  location.reload();
};

// Publish this browser's copy back to the server (`PUT /site.uapp`, see
// server.rs). `token` is the site's publish secret. The download's ETag goes
// along as If-Match, so a site that moved on since (another publish, a
// deploy) answers 409 instead of being overwritten; `force` drops that check.
// A copy with no ETag (the server sent none, or it predates keeping one) has
// nothing to check against, so it is refused unless forced: a bare PUT would
// silently replace whatever the server holds now. On success the server
// equals this copy, which therefore becomes the new ETag baseline for the
// sync pill. Resolves to the server's reply.
window.__uappSitePublish = async (token, { force = false } = {}) => {
  if (!siteState || !window.__uappSiteArchive) throw new Error("not a hosted site");
  if (!force && !siteState.etag) {
    const e = new Error("this copy has no record of which server version it came from");
    e.status = 428; // Precondition Required, as a server would say it
    e.noBaseline = true;
    throw e;
  }
  await window.__uappFlushSave();
  const body = await exportBytes(true);
  const headers = { authorization: "Bearer " + token, "content-type": "application/octet-stream" };
  if (siteState.etag && !force) headers["if-match"] = siteState.etag;
  const resp = await fetch(new URL(window.__uappSiteArchive, location.href), {
    method: "PUT", headers, body, cache: "no-store",
  });
  const text = await resp.text();
  let json = null;
  try { json = JSON.parse(text); } catch {}
  if (!resp.ok) {
    const e = new Error((json && json.error) || text || ("HTTP " + resp.status));
    e.status = resp.status;
    throw e;
  }
  siteState.etag = (json && json.etag) || resp.headers.get("etag") || "";
  kvSet(siteKey("etag"), siteState.etag);
  // The saved copy now equals the server: no local edits are pending.
  siteState.localSavedAt = 0;
  kvDel(siteKey("saved"));
  return json;
};

// A ?open= link to an archive on another site: ask before running it. It
// runs on THIS origin, in the same storage as every other app here (see
// docs/design/web-build-isolation.md), so the visitor should know whose code
// it is. Asked once per archive URL; the answer is kept with the copy.
async function allowForeignOpen(url) {
  if (OPEN.via !== "param") return true;
  const u = new URL(url, location.href);
  if (u.origin === location.origin) return true;
  const key = "uapp.site.allowed:" + siteIdFor(url);
  if (kvGet(key)) return true;
  const yes = await splashAsk(
    `Open an app from ${u.host}?`,
    "It will run in this browser with the same access as the apps you keep " +
    "here, including their stored data. Only open apps from sites you trust.",
    "Open app", "Cancel");
  if (yes) kvSet(key, "1");
  return yes;
}
