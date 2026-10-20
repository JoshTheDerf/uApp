# App isolation

An app's own JavaScript runs on the same origin as the shell that wraps it. This doc describes what that means today, what the October 2026 hardening pass closed anyway, and how to give the app frame an origin of its own. That last part is a design, not something that's been built yet.

## The trust model today

The desktop server listens on `http://127.0.0.1:<port>`. The shell is served at `/`, and the app is an iframe of `/app/` on that same origin. Both talk to the core over the same WebSocket (`/ws`), authenticated by the same HttpOnly cookie.

Same origin means the app can do anything the shell can:

- Open its own socket and call any RPC, including `ai.approve`, `ai.answer`, `config.set`, `mcp.add`, `chat.send`, `crypt.set` and `template.apply`.
- Reach into the shell's DOM through `parent.document`, read what the panels show, and click their buttons.
- Answer its own approval prompt. When an app calls a gated tool through `uapp.tool()`, the prompt it triggers is just another pending entry, and `ai.approve` with that id is an RPC the app can send too. The prompt is only a real check against an app that plays fair.

That's fine for an app you built yourself. It isn't fine for a `.uapp` someone sent you, which is the whole point of the format. Until the frame is isolated, opening a shared `.uapp` means trusting its author about as much as running a program they sent you (with the difference that it can't leave the sandbox the core gives it: no arbitrary files, no shell commands).

The browser build (`src/wasm.rs`, the demo and `uapp serve --chrome`) has the same shape. The shell, the worker that runs the engine, and the app frame all share one origin, and the service worker serves `/app/` from the copy in the browser.

## What this pass closed

These hold even with the app on the shell's origin:

- **Deny means deny.** An app-triggered tool call that is denied or times out fails with a "denied" error. It used to run anyway.
- **Secrets moved off the file.** The AI API key, MCP credentials and "always allow" answers live in `<config_dir>/uapp/secrets/<file key>.json` (owner-only, written atomically), keyed by the file's path. A shared file can't arrive pre-approved, a copy doesn't carry your key, and `config.get` returns `has_api_key` / `hasAuth` instead of the values. The key is bound to the provider and base URL it was saved for, and MCP headers to their server URL, so rewriting the config to point somewhere else drops the credential instead of sending it there. The browser build has nowhere else to put them and still keeps them in the file (redacted in `config.get`).
- **No arbitrary deletes.** `prefs.set` only takes the `shell` preference, and launcher removal only deletes entries the installer could have created.
- **No arbitrary programs.** `browser.open` takes http(s) and `mailto:` only.
- **Other localhost origins are refused.** Owner routes and the socket require `Sec-Fetch-Site` to be `same-origin` or `none` and `Origin`, when sent, to be exactly ours. Cookies aren't port-scoped, so a page on another loopback port used to be able to post to `/upload` with our cookie attached.
- **Framing.** Every response sends `frame-ancestors 'self'`, and the shell document gets a full CSP (`script-src 'self'`, no inline script).
- **Tokens.** 32 bytes from the OS CSPRNG, compared in constant time, dropped from the address bar once the cookie is set.
- **SSRF in the fetch tools.** `web.fetch` and `files.fetch` check every resolved address at connect time and follow redirects by hand, one checked hop at a time.

## What's still open

Everything in the first list above, mainly. An app can still:

- approve its own tool prompts (`ai.approve`) or answer `ask_user` questions;
- change the AI config. Moving the base URL drops the saved key, but with `provider: "local"` no key is needed, so the app can still send your chats (and with them your files and tables) to a server it picks;
- add an MCP server at any URL, local ones included. Listing its tools makes the core POST JSON to that URL, which is a request into your network that the fetch tools would have refused. Local endpoints are allowed on purpose, since a local Ollama or MCP server is a normal setup the user configured. The only real fix is that app code can't configure them, which is what isolation gives you;
- read chat history and everything else the shell can see.

## Design: the app frame on its own origin

### Origins

Serve the app frame from a second origin that shares nothing with the shell's:

| | Shell | App frame |
|---|---|---|
| Origin | `http://127.0.0.1:<port>` | `http://localhost:<port>` |
| Cookie | `uapp_t_<port>` (owner token) | `uapp_a_<port>` (app token) |

`localhost` and `127.0.0.1` are different sites, not just different origins, so a `SameSite=Strict` cookie set by one is never sent with requests from the other. Both count as secure contexts, so camera, microphone and the like keep working.

Two things to handle:

- `localhost` can resolve to `::1`. The server should listen on `[::1]:<port>` as well as `127.0.0.1:<port>`, or the shell should probe and fall back.
- A second port on 127.0.0.1 would also be a separate origin, but it's the same site, so the owner cookie would ride along with the app's requests. The checks added in this pass would refuse them, but `localhost` doesn't depend on getting those checks right.

Per-app subdomains (`<app_id>.localhost:<port>`) would separate apps from each other as well as from the shell. Chromium and Firefox resolve `*.localhost` to loopback themselves. WebKitGTK and Android's WebView need checking before relying on it.

The host check (`host_is_local`) already accepts `localhost`. The router needs to know which origin a request was made to, from its `Host`, and serve shell routes only on the shell origin and app routes only on the app origin.

### Tokens and what each one may call

The server mints an app token per session next to the owner token. The shell loads the frame as `http://localhost:<port>/app/?a=<app token>`; that response sets the app cookie (HttpOnly, `SameSite=Strict`, scoped to the app origin) and `uapp.js` drops `?a=` from the frame's URL, as `core.js` now does for `?t=`.

Each WebSocket connection is tagged with the token that opened it, and `rpc::dispatch` checks the method against the connection's role. An app connection gets an allowlist:

- `sql.query`, `sql.exec`, `sql.batch` (the core's authorizer keeps them off `uapp_*` and `sqlar`);
- `files.*` on the archive, `import.csv`;
- `tools.list`, `tools.call` (the approval prompt shows in the shell, and only an owner connection can answer it);
- action registration, invoke replies, `eval.result`, console forwarding;
- `app.info` (minus `path`), `toolbar.*`, `panel.set`, `files.present`, `app.reload`, `ping`;
- `browser.open`, behind a prompt.

Everything else (`ai.*`, `chat.*`, `sessions.*`, `config.*`, `mcp.*`, `prefs.*`, `crypt.set`, `template.*`, `app.save`, `app.openFile`, `app.quit`, shortcuts, `win.*`) is owner-only. Events get the same split: an app connection receives `changes`, `toolbar`, `panel`, `reload` and its own invocations, never `approval`, `question` or chat events.

`config.set` needs a closer look before it goes owner-only. `toolbar.setDefault` already covers the one config write apps are documented to make, but app code in the wild may use `config.*` for its own settings. A per-app namespace (`app.<key>`) on the allowlist would keep that working.

### Replacing same-origin reach with postMessage

The shell reaches into the frame in a few places. Each becomes a message, with both sides checking `event.origin` against the one origin they expect.

- **run_js in the app context.** Already independent of the parent: `uapp.js` in the frame runs `eval.invoke` from its own socket and replies with `eval.result`. It moves to the app socket unchanged.
- **Console capture.** Also already on the frame's own socket.
- **URL mirroring** (`main.js`, hosted-site mode). The frame posts `{__uappNav: href}` on load, `hashchange` and `popstate`, and the shell posts back `{__uappGo: path}` for back/forward instead of setting `contentWindow.location`. The frame's `src` can still be set from outside for reloads.
- **External link clicks** (`main.js`). `uapp.js` intercepts clicks on links to other origins and posts `{__uappOpen: href}`; the shell decides between `browser.open` and a top-level navigation as it does now.
- **Toolbar shortcut.** `toolbar-visibility.js` listens for keys in `contentDocument`. Instead, `uapp.js` listens for the configured shortcut and posts `{__uappToolbar: {action: "toggle"}}`, the message it already sends for `uapp.toolbar.*`. The shell sends the shortcut to the frame when it loads and when it changes.
- **The `Sec-Fetch-Dest: iframe` fallback** in `shell()` (a root `href="/"` followed inside the frame) goes away: on the app origin, `/` simply is the app.

### Permissions and the iframe

A cross-origin frame only gets powerful features its embedder delegates. The `allow` attribute on `#appframe` already names camera, microphone, display capture, geolocation and clipboard read, and those apply to the frame's own origin by default, so nothing changes there. Notifications have no Permissions-Policy token and can't be delegated to a cross-origin frame at all, so they would go through an RPC that the shell shows.

The native prompts in `permissions.rs` file grants per app id. The webviews report the requesting origin, which becomes the app origin, and the broker should accept that origin for the current app (and nothing else).

### Headers on the app origin

- `Content-Security-Policy: frame-ancestors http://127.0.0.1:<port>`, so only our shell can frame the app.
- The shell's `frame-src` names the app origin.
- No script policy for the app itself; it runs what its author wrote.

### Data that moves with the origin

`localStorage`, IndexedDB and cookies an app set belong to the old origin and won't be visible on the new one. Most apps keep their data in the `.uapp`, but some will have used `localStorage`. A one-time handoff would cover them: on first load after the switch, the shell (still on the old origin) reads the old origin's storage for that app and posts it to the frame, which writes it into its own.

### The browser build

The wasm build is a static bundle on one origin: the shell, the worker running the engine and the service worker that serves `/app/` from the browser's copy. There is no server to mint tokens or tag sockets, and a sandboxed iframe (`sandbox` without `allow-same-origin`) gets an opaque origin that no service worker controls, so the app's own files wouldn't load.

What works is a second origin the deployer provides, for example `apps.example.com` next to `example.com`, serving the same bundle:

- The app frame loads from the app origin, where a service worker serves `/app/` like today. It gets the files by asking its page, which relays to the shell over `postMessage` (the frame's `uapp.js` already speaks `postMessage` to the shell in wasm mode).
- The worker on the shell origin applies the same owner/app allowlist to messages by the frame they came from, which it can tell apart there, unlike sockets on the desktop.
- Without a configured app origin, the build falls back to today's single-origin mode, and the shell should say so (for example, in Settings: "This app isn't isolated from the editor").

This is the bigger part of the work and can come after the desktop server, where the change is mostly in `server.rs`, `rpc.rs` and the frame plumbing in `main.js` and `uapp.js`.

### Order of work

1. Server: listen on `[::1]` too, tell shell and app origins apart by `Host`, app token and cookie, role on each connection, the RPC and event allowlist.
2. `uapp.js` and the shell: the `postMessage` replacements above, the frame loaded from the app origin.
3. Permissions: accept the app origin in the native prompts.
4. `localStorage` handoff.
5. The browser build's second origin.

Until step 1 ships, the practical advice for users stays the same: open a `.uapp` from someone you don't trust the way you'd run a program from them.
