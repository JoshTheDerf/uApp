# App isolation in the browser build

The browser build (web/boot.js and friends) runs every app on the origin that serves the shell. The app frame, the scratch frame and the shell are all same-origin, and none of them is sandboxed, because the service worker can only serve an app's files to frames on its own origin. This page says what that means and what the shell does about it today.

## What the shell enforces

- **RPCs only from our own frames.** The postMessage router answers a message only when it comes from our origin and from a window inside this shell's document (the app frame, the scratch frame, the file viewer, or anything they embed). A page that frames the shell, a sibling frame, a popup and a third-party iframe inside an app all get nothing. Replies and events go out with our origin as the target.
- **`host.*` is for the launcher only.** Listing, opening, exporting and deleting the other apps stored in this browser works for the document the operator put on the page: the bundled launcher on a bare visit, the server's `--open` default, or a hosted site's own `/site.uapp`. A document named by a link (`?open=`, `?app=`) gets an error. The archive can't claim the role for itself.
- **Apps from other sites ask first.** A `?open=` link to an archive on another origin shows "Open an app from <host>?" before anything is downloaded. The answer is remembered for that archive URL.
- **The publish token stays in memory.** It's asked for once per page and kept inside sync-pill.js, not in sessionStorage.

## What it can't enforce

These are the limits of running everything on one origin. A malicious app can get around every rule above, because same-origin script access doesn't go through postMessage at all:

- An app can reach its parent directly (`window.parent`, `window.top`) and call into the shell's JavaScript, including the transport the shell itself uses.
- An app can open OPFS, IndexedDB and localStorage itself, so it can read and overwrite every app stored in this browser, and the launcher's index.
- An app can read anything the shell keeps in memory if it patches the right function first (a prompt that asks for the publish token, say).
- Any other page on the same origin (thederf.com runs several apps behind one proxy) can do the same to the shell.

So the checks above stop accidents and keep honest third-party content (ads, embeds) out. They don't make it safe to open an app you don't trust. The confirm step for other sites is there so that choice is made on purpose.

## What would fix it

Serving each app from an origin of its own (a per-app subdomain, or a sandboxed frame with an opaque origin) would make the browser enforce the boundary. Both need server and proxy work: the service worker can't serve files to a frame on another origin, so the app's files would have to come from that origin's own worker or server, and the shell would have to talk to the app only over postMessage. Until then, treat every app you open in the browser build as having access to all the others.
