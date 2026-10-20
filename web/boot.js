/* uapp browser build — main-thread bootstrap.
 *
 * Order of operations:
 *   1. Register the service worker (archive URLs + COOP/COEP for isolation).
 *   2. Start the wasm worker, hand it the SharedArrayBuffer bridge.
 *   3. Open what the URL says (see OPEN in env.js): a stored app (?app=<id>),
 *      an archive fetched from a URL (?open=<url>, or the page's default), a
 *      hosted site's own /site.uapp (site-chrome.js), or else the launcher —
 *      itself a .uapp with a grid of the OPFS app library and the samples
 *      (host.* API).
 *   4. Install window.__uappTransport and load the normal shell.
 *
 * The rest lives in uapp-boot/: env (what this page is), splash (boot UI,
 * notices, error card), storage (the app library), worker-bridge (the wasm
 * worker, the SAB bridge, the shell's transport), sw-responder (the service
 * worker and its archive asks), frame-router (pages in our frames, host.*),
 * autosave, site-sync (archives from a URL: reset, publish) and tabs (one
 * shell per document).
 *
 * What other files use from here, on window: __uappWasmHost and __uappBase
 * (uapp.js, the shell), __uappTransport (core.js), __uappFlushSave (main.js,
 * sync-pill.js), __uappSaveState, __uappSiteState, __uappSiteReset and
 * __uappSitePublish (sync-pill.js), __uappStandDown (site-chrome.js). Read
 * from window: __uappChrome, __uappSiteArchive, __uappSiteName and
 * __uappSiteEntry, which site-chrome.js sets before importing us.
 */

import { currentAppId, flushSave, onEngineDead, saveFailureBanner, scheduleSave, setCurrentApp } from "./uapp-boot/autosave.js?v=__UAPP_BUILD__";
import { BASE, IS_LAUNCHER, OPEN, TAB_KEY, errText, lastInfoName, markLeaving, setAppName } from "./uapp-boot/env.js?v=__UAPP_BUILD__";
import { switchTo } from "./uapp-boot/frame-router.js?v=__UAPP_BUILD__";
import { openSiteArchive, siteState } from "./uapp-boot/site-sync.js?v=__UAPP_BUILD__";
import { errorCard, fetchWithProgress, splashEl, splashHide, splashNote, splashShow, splashStatus } from "./uapp-boot/splash.js?v=__UAPP_BUILD__";
import { appRead, appsIndex, indexTouch, migrateLegacy } from "./uapp-boot/storage.js?v=__UAPP_BUILD__";
import { serverMovedOn, startServiceWorker } from "./uapp-boot/sw-responder.js?v=__UAPP_BUILD__";
import { claimDocument } from "./uapp-boot/tabs.js?v=__UAPP_BUILD__";
import { deliverEvent, failAppReady, installTransport, markAppReady, openApp, sendRpc, startWorker } from "./uapp-boot/worker-bridge.js?v=__UAPP_BUILD__";

function onWorkerEvent(envelope) {
  const params = envelope.params || {};
  // Closing the document (the shell's ✕ / app.quit) goes back to the landing
  // chooser: save one last time, then reload with the chooser forced on.
  // A failed final save keeps the document open, with the failure notice
  // offering a download (or closing anyway): navigating away would throw the
  // unsaved work out with the engine.
  if (params.type === "sync" && params.state === "closed") {
    (async () => {
      if (await flushSave()) switchTo(null); // back to the launcher
      else saveFailureBanner(true);
    })();
    return; // don't let the shell paint its "closed" overlay first
  }
  if (params.type === "changes") {
    scheduleSave();
    // For the hosted-site chrome (outside the module graph): "your copy changed".
    try { window.dispatchEvent(new CustomEvent("uapp-changes", { detail: params })); } catch {}
  }
  if (params.type === "renamed" && params.name) {
    setAppName(params.name);
    if (currentAppId && !siteState) indexTouch(currentAppId, params.name);
  }
  if (params.type === "open_url" && params.url) {
    try { window.open(params.url, "_blank", "noopener"); } catch {}
  }
  deliverEvent(envelope);
}

// Boot could not finish. Said on the splash, with a way to try again: a
// production page opens its app with ?open= or the --open default, and a
// silent failure there is an endless "Loading…".
function fatal(msg, title = "uApp could not start", retryLabel = "Try again") {
  msg = String(msg || "Something went wrong.");
  console.error("uapp:", msg);
  // Hosted-site chrome (site-chrome.js): the body is the reader's page, and
  // the right failure mode is to leave it alone. Report, don't repaint.
  if (window.__uappChrome) {
    window.dispatchEvent(new CustomEvent("uapp-boot-fatal", { detail: msg }));
    return;
  }
  const buttons = [[retryLabel, () => { markLeaving(); location.reload(); }]];
  if (!IS_LAUNCHER) buttons.push(["Back to your apps", () => switchTo(null)]);
  errorCard(title, msg, buttons);
}

// ---- boot ------------------------------------------------------------------------

(async () => {
  try {
    // Asked for now, waited on just before the stored copy is read: the old
    // owner's last save runs while this shell starts its workers.
    const docLock = claimDocument();
    // A document switch is a navigation, so name the app we are heading for
    // instead of showing a generic "Starting".
    if (splashEl && OPEN.kind === "app") {
      const e = appsIndex().find((a) => a.id === OPEN.id);
      if (e) splashStatus.textContent = "Opening " + e.name + "…";
    }
    await startServiceWorker();
    await startWorker({ onEvent: onWorkerEvent, onDead: onEngineDead });
    await migrateLegacy();

    // Open what the URL names (OPEN), or the launcher (itself a .uapp).
    let info = null;
    if (TAB_KEY) {
      const slow = setTimeout(() => splashNote("Waiting for another tab to save this document…"), 400);
      await docLock;
      clearTimeout(slow);
    }
    if (OPEN.kind === "app") {
      const entry = appsIndex().find((a) => a.id === OPEN.id);
      const wantName = (entry && entry.name) || "App";
      splashShow("Opening " + wantName + "…", "Opening the document…");
      const bytes = await appRead(OPEN.id); // null = brand-new app (host.create)
      try {
        info = await openApp(bytes, wantName);
        setCurrentApp(OPEN.id);
        if (!bytes) scheduleSave(); // persist the fresh app right away
      } catch (e) {
        // Not the launcher in its place: on this URL it would have no host.*
        // (see IS_LAUNCHER). The error card offers the way back.
        throw new Error("Could not open the app: " + errText(e));
      }
    } else if (window.__uappSiteArchive) {
      // Saved under its own id (siteState) from the first change on.
      info = await openSiteArchive(window.__uappSiteArchive);
      if (!info) { switchTo(null); return; } // an app from another site, declined
    } else {
      // The launcher is never saved (no current app), so a reload starts
      // over from what the server serves.
      splashShow("Starting uapp…", "Loading the app library…");
      const bytes = await fetchWithProgress(new URL("launcher.uapp", BASE), "the launcher app");
      info = await openApp(bytes, "Apps");
    }
    setAppName(info.name || "uapp");
    document.title = `${lastInfoName} — uapp`;
    markAppReady();
    installTransport();

    // The iframes in index.html start empty: their archive URLs only resolve
    // once the service worker controls the page and an app is open. Point them
    // at the real (base-relative) paths now.
    splashNote("Loading the app…");
    const appFrame = document.getElementById("appframe");
    // Hold the splash until the app has actually painted, with a cap in case
    // the frame never fires load.
    const framePainted = new Promise((resolve) => {
      if (!appFrame) return resolve();
      appFrame.addEventListener("load", resolve, { once: true });
      setTimeout(resolve, 8000);
    });
    // Only the app frame: the scratch frame is loaded on demand (see
    // loadScratchFrame). No cache-buster — these are served no-store, and a
    // stable url lets later reloads replace this history entry instead of
    // pushing a new one (see reloadAppFrame in shell/main.js).
    if (appFrame) appFrame.src = window.__uappBase + (window.__uappSiteEntry || "app/");

    // Open with the toolbar this app saved for itself (see toolbar.rs), before
    // the shell's modules run: applied a round trip later, the bar paints and
    // then vanishes — a glitch on every single open. toolbar-visibility.js
    // reads this back and owns it from here.
    try {
      const tb = await sendRpc("toolbar.get", {});
      if (tb && tb.hidden) document.getElementById("topbar")?.classList.add("hidden");
    } catch {}

    await import(new URL("shell/main.js", BASE));
    await framePainted;
    splashHide();
    // An archive that came from a URL can go back to it: the sync pill in the
    // topbar (local edits, server moved on, Publish). Outside the module graph
    // so the hosted-site chrome and this page share one implementation.
    if (siteState) {
      try { (await import(new URL("sync-pill.js", BASE))).startSyncWatch(); }
      catch (e) { console.warn("uapp: sync pill unavailable:", e); }
    }
  } catch (e) {
    console.error(e);
    failAppReady(e);
    if ((e && e.updated) || await serverMovedOn()) {
      fatal("A new version was deployed while this page was loading. Reload to get it.", "uApp was updated", "Reload");
    } else {
      fatal(errText(e));
    }
  }
})();
