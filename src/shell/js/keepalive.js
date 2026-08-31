/* Android background chat keep-alive.
 *
 * When any session's AI run is live, tell the native side to hold the process
 * out of the cached-apps freezer (ShellBridge.chatKeepalive starts a dataSync
 * foreground service; see ChatWorkService.kt). Without it, switching apps
 * froze the run's server thread mid-turn and killed its provider socket, so
 * every backgrounded chat ended in a connection error.
 *
 * The "off" edge here can freeze with the page — the service double-checks
 * the core's /health itself and stops on its own once no run is live. */

import { on } from "./core.js";

const bridge = () => window.UAppAndroid;
export const keepaliveSupported = () =>
  typeof bridge()?.chatKeepalive === "function";

const live = new Set(); // sessions with a run in flight
let active = false;

function set(a) {
  if (a === active || !keepaliveSupported()) return;
  active = a;
  try { bridge().chatKeepalive(a); } catch {}
}

// Per-session run state, straight from the server's ai events. "retry" counts
// as live: the loop is waiting out a transient provider failure.
const LIVE_STATES = ["running", "thinking", "streaming", "tool", "retry"];
on("ai", (p) => {
  const s = p.session || "main";
  if (s.startsWith("compact:")) return; // internal helper, never a real run
  if (LIVE_STATES.includes(p.state)) live.add(s);
  else live.delete(s);
  set(live.size > 0);
});

// Authoritative resync (covers events lost while the page was suspended).
on("info", (info) => {
  const busy = info.aiSessions || [];
  live.clear();
  for (const s of busy) live.add(s);
  set(live.size > 0);
});
