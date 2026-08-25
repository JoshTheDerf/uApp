/* uapp hosted-site chrome — injected into every page a `uapp serve` site
 * returns (see inject_chrome in src/public.rs).
 *
 * The server's HTML is the plain document: crawlers and anything without JS
 * get rendered pages with no engine, no service worker and no iframe. This
 * file then MORPHS that page in place as soon as it loads: boot the wasm
 * engine on a copy of the site downloaded from /site.uapp, put the normal
 * uapp shell in the body, and point its app frame at the page you were already
 * reading. From there it is the ordinary shell — chat, SQL browser, file
 * browser, the site's own build pipeline — all against the in-browser copy.
 *
 * Nothing is persisted (boot.js leaves currentAppId null, its "ephemeral,
 * never saved" path), so a reload throws the copy away and comes back from the
 * server's authoritative one. That is the whole contract: edit freely, reload
 * to get the real site back.
 *
 * There is no loader. The server's page stays on screen, untouched, while the
 * shell boots invisibly beneath it; the swap happens only once the shell's
 * frame has painted the same page, so entry is close to seamless. If the
 * editor cannot start (no service worker, wasm blocked, a fetch failure, or
 * simply too long) the plain page is what remains — a JS-running crawler with
 * no service-worker support indexes the page, not a splash. `#noedit` (or
 * ?noedit) in the URL opts out entirely.
 */

(() => {
  const SELF_SRC = (document.currentScript && document.currentScript.src) || "/chrome.js";
  // Only the top document gets chrome. Inside the shell's own app frame the
  // page is served by the service worker and gets uapp.js instead; injecting
  // here too would stack a second editor inside the editor.
  if (window.top !== window || window.__uappWasm || window.__uappChrome) return;
  window.__uappChrome = true;

  // ---- the shell's DOM ------------------------------------------------------
  // Same structure as the browser build's index.html; boot.js and the shell
  // modules look these ids up by name. No #boot-splash: every splash helper in
  // boot.js is a no-op without the element, and the page underneath IS the
  // loading state here.
  const ROOT_ID = "uapp-shell-root";
  const SHELL_BODY = `
<shell-topbar id="topbar"></shell-topbar>
<main id="main">
  <iframe id="appframe"></iframe>
  <iframe id="scratchframe" style="display:none" title="scratchpad"></iframe>
  <chat-panel id="sidebar" class="overlay-panel"></chat-panel>
  <sql-panel id="sqlpanel" class="overlay-panel"></sql-panel>
  <files-panel id="filebrowser" class="overlay-panel"></files-panel>
  <settings-panel id="settingspanel" class="overlay-panel"></settings-panel>
  <tools-panel id="toolspanel" class="overlay-panel"></tools-panel>
</main>
<file-viewer></file-viewer>`;

  // The shell normally IS the body (shell.css lays body out as a flex column).
  // Here it lives in a fixed full-viewport root that stands in for the body,
  // so it can boot invisibly beneath the still-visible page and never has to
  // be re-parented (moving a custom element re-runs connectedCallback).
  const ROOT_CSS = `
#${ROOT_ID}{position:fixed;inset:0;z-index:2147483000;display:flex;flex-direction:column;
  background:var(--bg,#fff);color:var(--text,#111);font:14px/1.45 system-ui,sans-serif}
#${ROOT_ID}[hidden]{display:flex;visibility:hidden}`;

  function addStyle(css) {
    const el = document.createElement("style");
    el.textContent = css;
    el.dataset.uapp = "1";
    document.head.appendChild(el);
    return el;
  }

  // shell.css is appended INERT (media="not all"): its body/html rules would
  // restyle the page that is still on screen. It is switched on at the swap.
  function loadCssInert(href) {
    return new Promise((resolve, reject) => {
      const l = document.createElement("link");
      l.rel = "stylesheet";
      l.href = href;
      l.media = "not all";
      l.dataset.uapp = "1";
      l.onload = () => resolve(l);
      l.onerror = () => reject(new Error("could not load " + href));
      document.head.appendChild(l);
    });
  }

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = src;
      s.dataset.uapp = "1";
      s.onload = resolve;
      s.onerror = () => reject(new Error("could not load " + src));
      document.head.appendChild(s);
    });
  }

  // ---- the entry path -------------------------------------------------------
  // The frame opens on whatever the reader was looking at. `/app/` is a ROUTE
  // into the archive, not a stored prefix (see handleSwRequest in boot.js), so
  // "/how-do-bikes-work/" becomes "app/how-do-bikes-work/" and the archive's
  // own index/extension resolution takes it from there.
  function entryPath() {
    return "app/" + location.pathname.replace(/^\/+/, "");
  }

  // ---- title --------------------------------------------------------------------
  // The tab keeps showing the PAGE's title, not "<site> — uapp": the frame's
  // title is mirrored up on every load and whenever the page changes it, and
  // the shell's own writes (core.js, topbar.js, boot.js) are overridden.
  let wantTitle = document.title;
  let frameTitleObs = null;
  function pinTitle() {
    if (wantTitle && document.title !== wantTitle) document.title = wantTitle;
  }
  function mirrorFrameTitle(frame) {
    let doc;
    try { doc = frame.contentDocument; } catch { return; }
    if (!doc) return;
    const apply = () => { const t = (doc.title || "").trim(); if (t) { wantTitle = t; pinTitle(); } };
    apply();
    if (frameTitleObs) frameTitleObs.disconnect();
    const titleEl = doc.querySelector("title") || doc.head;
    if (titleEl) {
      frameTitleObs = new MutationObserver(apply);
      frameTitleObs.observe(titleEl, { childList: true, characterData: true, subtree: true });
    }
  }
  // The parent <title> is watched too, so the shell's `${name} — uapp` never
  // sticks (it would otherwise win the race after app.info).
  new MutationObserver(pinTitle).observe(document.head, { childList: true, subtree: true, characterData: true });

  // ---- morph ----------------------------------------------------------------
  let starting = false;
  const FALLBACK_MS = 45000; // a slow connection still fits; a stuck boot does not

  // Nothing was touched on the visible page, so standing down is just removing
  // what we added.
  function abandon(why) {
    console.warn("uapp: editor did not start — leaving the plain page" + (why ? ": " + why : ""));
    for (const el of document.querySelectorAll("[data-uapp]")) el.remove();
    const root = document.getElementById(ROOT_ID);
    if (root) root.remove();
  }

  // The swap: the frame has painted the same page the reader is looking at, so
  // the shell can take over without a visible seam.
  // Snapshotted BEFORE anything of ours is added: the shell's components
  // install their own <style> elements (installStyle in ui.js) with no marker,
  // and disabling those would strip the shell itself.
  let pageSheets = [];
  function reveal(shellCss) {
    for (const el of pageSheets) {
      // The page's own `body { max-width: 40em; margin: 3em auto }` and friends
      // would squeeze the shell into a column. Off, not removed — a moved
      // <style> still applies.
      try {
        if (el.sheet) el.sheet.disabled = true;
        else if (el.tagName === "LINK") el.disabled = true;
      } catch {}
    }
    shellCss.media = "all";
    const root = document.getElementById(ROOT_ID);
    for (const el of [...document.body.children]) if (el !== root) el.remove();
    document.documentElement.removeAttribute("style");
    document.documentElement.className = "";
    document.body.removeAttribute("style");
    document.body.className = "";
    if (root) root.hidden = false;
  }

  async function enterEditor() {
    if (starting) return;
    starting = true;
    try {
      window.__uappSiteArchive = "/site.uapp";
      window.__uappSiteName = (document.title.split(" — ")[0] || "Site").trim().slice(0, 80);
      window.__uappSiteEntry = entryPath();
      pageSheets = [...document.querySelectorAll('style, link[rel~="stylesheet"]')];

      addStyle(ROOT_CSS);
      const root = document.createElement("div");
      root.id = ROOT_ID;
      root.hidden = true;
      root.innerHTML = SHELL_BODY;
      document.body.appendChild(root);

      // shell.css and the icon sprite are what the shell's components expect
      // to be present; boot.js imports the components themselves.
      const [shellCss] = await Promise.all([loadCssInert("/shell.css"), loadScript("/icons.js")]);

      const frame = document.getElementById("appframe");
      let revealed = false;
      frame.addEventListener("load", () => {
        if (!frame.getAttribute("src")) return; // the initial about:blank
        mirrorFrameTitle(frame);
        if (!revealed) { revealed = true; clearTimeout(guard); reveal(shellCss); startSyncWatch(); }
      });

      // No frame in time means the engine is not coming (no service worker,
      // wasm blocked, a hung fetch): the plain page simply stays.
      const guard = setTimeout(() => { if (!revealed) abandon("timed out"); }, FALLBACK_MS);
      // boot.js reports a fatal error by event in site mode (it must not touch
      // the body — that is the page the reader is on).
      window.addEventListener("uapp-boot-fatal", (e) => {
        clearTimeout(guard);
        if (!revealed) abandon(e.detail);
      }, { once: true });

      // Same ?v= as our own tag, so a deploy busts the CDN/browser cache of
      // boot.js too (boot.js loads the rest relative to itself, uncached by
      // the SW and revalidated by ETag; the entry points are what go stale).
      const v = new URL(SELF_SRC, location.href).searchParams.get("v");
      await import("/boot.js" + (v ? "?v=" + v : ""));
    } catch (e) {
      starting = false;
      console.error("uapp: could not start the editor", e);
      abandon(String((e && e.message) || e));
    }
  }

  // ---- server/client sync -------------------------------------------------------
  // The copy in this tab was downloaded once; the server's may move on (a
  // deploy, an edit from elsewhere). Only THAT case gets a pill — local edits
  // on top of a current server copy are just the reader working, nothing to
  // nag about. The stamp is the server's x-uapp-modified (newest file in the
  // archive), read with a HEAD; the baseline is the stamp at download time.
  let serverBaseline = 0;     // server stamp when this copy was downloaded
  let serverNow = 0;          // latest stamp seen
  let localEditAt = 0;        // ms; last change made in this tab
  let pill = null;
  window.addEventListener("uapp-changes", () => { localEditAt = Date.now(); renderPill(); });

  async function serverStamp() {
    try {
      const r = await fetch("/site.uapp", { method: "HEAD", cache: "no-store" });
      return Number(r.headers.get("x-uapp-modified")) || 0;
    } catch { return 0; }
  }
  function ago(ms) {
    const s = Math.max(0, Math.round(ms / 1000));
    if (s < 60) return "just now";
    const m = Math.round(s / 60); if (m < 60) return m + " min ago";
    const h = Math.round(m / 60); if (h < 48) return h + " h ago";
    return Math.round(h / 24) + " days ago";
  }
  // Two looks: a quiet grey "Local edits · N ago · Reset" whenever this tab
  // has changed its copy (the server is not newer — just a reminder that
  // what you see is yours, and that a reload throws it away), and an amber
  // "Server copy is newer" when the server has moved on since the download.
  function renderPill() {
    const bar = document.getElementById("topbar");
    if (!bar) return;
    const stale = serverBaseline && serverNow > serverBaseline;
    const local = localEditAt > 0;
    if (!stale && !local) { if (pill) { pill.remove(); pill = null; } return; }
    if (!pill) {
      pill = document.createElement("span");
      pill.id = "site-sync-pill";
    }
    // The topbar re-renders its innerHTML (app.info, language, …) and drops
    // foreign children; put the pill back whenever it is missing.
    if (!pill.isConnected) {
      const anchor = bar.querySelector(".spacer");
      if (anchor && anchor.nextSibling) bar.insertBefore(pill, anchor.nextSibling); else bar.appendChild(pill);
    }
    pill.className = stale ? "stale" : "local";
    pill.style.cssText = stale
      ? "display:inline-flex;align-items:center;gap:8px;padding:3px 6px 3px 10px;border-radius:999px;" +
        "background:#8a6400;color:#fff;font-size:12px;line-height:1.2;white-space:nowrap"
      : "display:inline-flex;align-items:center;gap:6px;padding:1px 4px 1px 8px;border-radius:999px;" +
        "background:rgba(255,255,255,.1);color:var(--chrome-dim,#bbb);font-size:11px;line-height:1.2;white-space:nowrap";
    pill.innerHTML = "";
    const txt = document.createElement("span");
    const btn = document.createElement("button");
    btn.onclick = () => (window.__uappSiteReset ? window.__uappSiteReset() : location.reload());
    if (stale) {
      const yours = local ? "your edits " + ago(Date.now() - localEditAt) : "your copy has no edits";
      txt.textContent = "Server copy is newer (updated " + ago(Date.now() - serverNow * 1000) + "; " + yours + ")";
      txt.title = "The site on the server changed after your copy was downloaded. Resetting discards the edits kept in this browser and reloads the server's version.";
      btn.textContent = "Reset to server copy";
      btn.style.cssText = "font:inherit;font-size:12px;padding:2px 8px;border-radius:999px;border:1px solid rgba(255,255,255,.6);background:rgba(255,255,255,.15);color:#fff;cursor:pointer";
    } else {
      txt.textContent = "Local edits · " + ago(Date.now() - localEditAt);
      txt.title = "You are working on your own copy of the site, kept in this browser across reloads. Nothing is sent to the server.";
      btn.textContent = "Reset";
      btn.title = "Discard your local copy and return to the server's version";
      btn.style.cssText = "font:inherit;font-size:11px;padding:1px 6px;border-radius:999px;border:1px solid rgba(255,255,255,.25);background:transparent;color:inherit;cursor:pointer";
    }
    pill.append(txt, btn);
  }
  async function checkServer() {
    const st = await serverStamp();
    if (!st) return;
    if (!serverBaseline) serverBaseline = st;
    serverNow = st;
    renderPill();
  }
  window.__uappSyncState = () => ({ serverBaseline, serverNow, localEditAt, pill: !!pill });
  function startSyncWatch() {
    // boot.js has the copy open by now: its download stamp is the baseline
    // (persisted with the saved copy, so it survives reloads), and a saved
    // copy's last save time is when "your edits" happened.
    const st = window.__uappSiteState;
    if (st) {
      serverBaseline = st.baseline || 0;
      if (st.hasLocal) localEditAt = st.localSavedAt || Date.now();
    }
    renderPill();
    const bar = document.getElementById("topbar");
    if (bar) new MutationObserver(() => { if (pill && !pill.isConnected) renderPill(); }).observe(bar, { childList: true });
    checkServer();
    setInterval(checkServer, 60000);
    setInterval(() => { if (pill) renderPill(); }, 30000);
    document.addEventListener("visibilitychange", () => { if (!document.hidden) checkServer(); });
    window.addEventListener("focus", checkServer);
  }

  // ---- go ---------------------------------------------------------------------
  function shouldEnter() {
    if (/(^|[#?&])noedit(=|&|$)/.test(location.hash + location.search)) return false;
    // No service worker means no way to serve the local copy's files at all.
    if (!("serviceWorker" in navigator)) return false;
    return true;
  }

  function start() {
    if (!shouldEnter()) return;
    enterEditor();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
