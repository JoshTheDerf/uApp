# App isolation

On the desktop and Android, an app's own JavaScript runs on an origin of its own, apart from the uApp editor (the shell) around it. It can use its own data and files and the tools, but it can't reach into the shell, change your AI or MCP setup, read your chats, or answer its own approval prompts. This page describes how that works and where it doesn't apply.

## Two origins

The server listens on `127.0.0.1:<port>` and `[::1]:<port>` and tells requests apart by their `Host`:

| | Shell | App frame |
|---|---|---|
| Origin | `http://127.0.0.1:<port>` | `http://localhost:<port>` |
| Token | owner token (`?t=`, cookie `uapp_t_<port>`) | app token (`?a=`, cookie `uapp_a_<port>`) |
| Routes | `/` (the shell), `/shell/*`, `/upload`, the downloads, `/ws` | `/` (the app), `/app/*`, root-level files, `/uapp.js`, `/scratch/`, `/ws` |

`localhost` and `127.0.0.1` are different sites, so the owner cookie (`SameSite=Strict`) never goes along with the app's requests and neither page can script the other. Both are secure contexts, so camera, microphone and the like keep working.

The shell's routes 404 on the app origin, and the app's root-level files and the scratch page 404 on the shell's. The shell can still read the archive under `/app/` on its own origin (file previews, downloads), but HTML and SVG from there are served with `sandbox`, so a page opened that way never runs as the shell. "Open in new tab" opens the app origin instead.

### Recognizing the app's requests

The shell loads the frame cross-site, so that load (and every reload the shell starts) carries `?a=<app token>`. uapp.js drops it from the frame's address (only a 64-digit hex value, so an app's own `?a=` is left alone) and keeps it in sessionStorage for that tab's later pages. After that:

- The page's own requests (scripts, images, fetches, links inside the frame) say `Sec-Fetch-Site: same-origin`. On an engine too old to send that header, a `Referer` on the app origin does instead, since app pages are served with `Referrer-Policy: same-origin`. A typed-in URL (`none`) is accepted too.
- The socket needs the app token and the app's own `Origin`.
- The app cookie is set on the `?a=` load, but browsers drop `SameSite=Strict` cookies in a cross-site frame (and several block third-party cookies outright), so it mostly helps non-browser clients and a page opened in a tab of its own.

App responses carry `Content-Security-Policy: frame-ancestors 'self' http://127.0.0.1:<port>`, and the shell's `frame-src` names the app origin.

### When the app stays on the shell's origin

If `[::1]:<port>` is taken by another program, `localhost` could reach that program (with the app token) instead of us, so the app shares the shell's origin for that session. The same goes for a machine without IPv6 where `localhost` doesn't resolve to 127.0.0.1. (No IPv6 at all is fine otherwise: `localhost` is 127.0.0.1 then.) Settings says when the app isn't isolated.

## What an app connection may call

Each socket has a role, owner or app, from the token that opened it, and `rpc::dispatch_as` checks app connections against `rpc::app_may_call`:

- `sql.query`, `sql.exec`, `sql.batch` (the SQL sandbox still applies);
- `files.*` and `import.csv`;
- `tools.list` and `tools.call`. A gated tool's prompt shows in the shell, and only an owner connection can answer it. `agent_run` and `agent_send` ask first too, since they run on your AI provider;
- `app.info` (without the file's `path`), `app.reload`, `toolbar.*`, `panel.set` and `ping`;
- `config.get` and `config.set` on `app.<name>` keys;
- `notify.show`.

The transport messages (`ctx.*`, `actions.*`, `eval.result`, `log.write`) work as before. Everything else is owner-only: `ai.*`, `chat.*`, `sessions.*`, the rest of `config.*`, `mcp.*`, `prefs.*`, `crypt.set`, `template.*`, `app.save`, `app.openFile`, `app.quit`, `app.rename`, `drop.ingest`, `eval.run`, `console.read`, `browser.open`, `web.*`, the launcher shortcuts and `win.*`.

Events are split the same way. App connections get `changes`, `present`, `toolbar`, `panel` and `reload`, plus the invocations addressed to them. Approval prompts, questions, chat and the rest go to owner connections only.

## The frame and the shell talk with postMessage

Both sides check `event.origin` against exactly the other's origin, and the shell also requires the sender to be inside its own frames (`src/shell/js/frame.js` and `uapp.js`).

- **`__uappNav`** (frame to shell): the page's URL, on load, `hashchange` and `popstate`. The shell keeps it for reloads, which go through `contentWindow.location.replace()` with the token (allowed across origins, and it doesn't add history entries). On a hosted site it also mirrors the address bar, and back/forward drive the frame the same way.
- **`__uappShell`** (shell to frame): the toolbar shortcut, the panel shortcuts and the link policy. uapp.js matches the shortcuts itself, the toolbar's in the capture phase and the panels' and Esc only when the app didn't use the key, and sends hits back as `__uappToolbar`.
- **`__uappToolbar`** (frame to shell): `uapp.toolbar.*`, `uapp.panel.*` and the forwarded shortcuts. `state` is answered with `__uappToolbarState`.
- **`__uappOpen`** (frame to shell): a click on a link to another site. The native window opens it in your browser, and a hosted site's editor leaves for it.
- **`__uappToken`**: asks for the token when sessionStorage lost it. The shell answers with `__uappTokenIs`.
- **`__uappStorage`**: the localStorage handoff below.

Either page may be ready first, so the frame says hello on load and again when it first hears from the shell, and the shell sends its config to the frame when it starts. On a hosted site the frame can also land on one of the server's own pages, which has no uapp.js; that frame shares the shell's origin, so the shell reads it directly.

run_js and console capture never went through the parent: uapp.js answers `eval.invoke` and forwards the console on its own socket. The scratch frame (run_js's "scratchpad" context) is on the app origin, so code the assistant writes there has the app's role.

## Permissions

The frame's `allow=` delegates camera, microphone, display capture, geolocation and clipboard read to the app origin. The broker in `permissions.rs` knows the open app's two origins and refuses a request from any other origin without asking. WebView2 reports the requesting origin. WebKitGTK doesn't, so on Linux a request counts as the open app's (the shell never asks for anything, and only the app frame gets these delegated). macOS, iOS and Android are left to the OS, as before.

Notifications can't be delegated to a cross-origin frame, so apps call `uapp.notify(title, {body})` (the `notify.show` RPC) and the shell shows it, as a system notification when it's allowed and as a toast otherwise.

## Data that moved with the origin

localStorage an app wrote used to land in the shell's storage. On the first load with the app on its own origin, the shell hands every key that isn't its own (`uapp.*`) to the frame, which writes them and reloads once, and then the shell deletes them. The port is random unless you pass `--port`, so this only finds what an earlier session on the same port left. IndexedDB and cookies aren't moved.

## Where it doesn't apply

- **The browser build and `uapp serve`** (`src/wasm.rs`, the demo, and a served site's editor) keep everything on one origin. There's no server to mint tokens or tag sockets, and a sandboxed frame gets an opaque origin that no service worker controls, so the app's files wouldn't load. Settings there says "This app isn't isolated from the editor. Only open apps you trust here." A second origin the deployer provides (`apps.example.com` next to `example.com`) would fix it; see [web-build-isolation.md](web-build-isolation.md).
- **Apps from each other.** Every app on a port shares `localhost:<port>`. Per-app subdomains (`<app id>.localhost`) would separate them, but WebKitGTK and Android's WebView need checking first.
- **A tab left open after its server exits.** If a later server for another app gets the same port, that old page can read the new app's files with same-origin requests. It can't open a socket, since the token changed. Ports are random, so this takes bad luck and a browser tab (the native window closes with its server).

## Also in place

These hold whichever origin the app is on:

- An app-triggered tool call that is denied or times out fails with a "denied" error.
- The AI API key, MCP credentials and "always allow" answers live in `<config_dir>/uapp/secrets/<file key>.json`, not in the file, and `config.get` returns `has_api_key` / `hasAuth` instead of the values. A key only goes to the provider and base URL it was saved for. (The browser build keeps them in its copy of the file, redacted in `config.get`.)
- `prefs.set` only takes the `shell` preference, launcher removal only deletes entries uApp made, and `browser.open` takes http(s) and `mailto:` only.
- Owner routes and the owner socket require `Sec-Fetch-Site` to be `same-origin` or `none` and `Origin`, when sent, to be exactly the shell's, so a page on another loopback port can't use the owner cookie.
- Tokens are 32 bytes from the OS CSPRNG, compared in constant time and dropped from the address bar once used.
- `web.fetch` and `files.fetch` check every resolved address at connect time and follow redirects one checked hop at a time.
