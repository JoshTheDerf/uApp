# Changelog

Notable changes to uApp. The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and uApp uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `uapp export`, `uapp info`, `uapp uninstall` and `uapp completions` commands. See [docs/CLI.md](docs/CLI.md).
- Master passwords can come from `--password-stdin`, `--password-file` or `UAPP_PASSWORD`, and `uapp passwd` takes the new one from `UAPP_NEW_PASSWORD`.
- `-v`, `-q` and `UAPP_LOG` control logging, and the window build also writes a log file (with any crash) to the per-user data folder.
- `uapp library DIR` serves a folder of .uapp files that browsers can open and publish back to.
- `uapp serve --chrome DIR` without a file hosts the browser build on its own, opening whatever `?open=` or `?app=` names.
- Apps can open with the toolbar hidden. The setting and its toggle key (`F9` by default) are saved in the file, and a corner handle brings the toolbar back.
- Dark mode (Settings > This device > Appearance), and on wide screens a panel can be pinned beside the app.
- Keyboard shortcuts for the panels: Ctrl+J (Cmd+J on macOS) for the chat, Ctrl+Shift+F for files and Ctrl+Shift+D for the database. Esc inside the app closes the open panel.
- The database grid, tabs, menus and dialogs work from the keyboard and with screen readers.
- On touch screens, a tap opens a file in the Files panel and a long press opens its menu.
- A new app's empty chat suggests things to build.
- The app API has `uapp.get`, typed errors (`uapp.UappError` with `.code` and `.method`), unsubscribe functions from `onChange` and friends, and editor types at `/uapp.d.ts`. See [docs/APP-API.md](docs/APP-API.md).
- `/health` on every server, and a health check in the Docker image.
- Public sites answer byte-range requests, so audio and video can seek.
- Release builds include a `.deb` and an `.AppImage` for Linux and a `.dmg` for macOS, all of which open .uapp files, plus `SHA256SUMS`.
- Docs for the [file format](docs/FORMAT.md), the [app API](docs/APP-API.md), [security](SECURITY.md) and [contributing](CONTRIBUTING.md).

### Changed

- uApp is one program, `uapp`. It opens the window (or your browser, in a build without the `gui` feature) and runs every command.
- A first argument that isn't a command, doesn't end in `.uapp` and doesn't exist is now a usage error, so a typo no longer creates a file. Exit codes are 0 for success, 1 for a failure and 2 for a usage error.
- The AI API key, MCP credentials and "always allow" answers are kept on your device instead of in the file. Opening an older file moves the key and MCP credentials out of it. A saved key only goes to the provider and base URL it was entered for.
- .uapp files now carry a format version. Opening a file checks it first, upgrades an older file in place, and refuses a file from a newer uApp or another program instead of guessing.
- Writes keep SQLite's rollback journal on disk next to the file (`<name>.uapp-journal`, there only while a write commits), so a crash mid-write is rolled back the next time the file opens.
- Snapshots are kept per file path, and opening a file that hasn't changed doesn't take another one.
- Encrypting or decrypting a file keeps the header that marks it as a .uapp.
- The shell has a refreshed look, and Settings is split into "This app" and "This device".
- The shell follows your browser's language when you haven't picked one, and everything in it is translated into all five languages.
- The Docker image no longer publishes `data/` in `/site.uapp` by default (add `--publish-data` to `UAPP_SERVE_FLAGS` to get it back).
- Servers shut down properly on SIGTERM (taking a final snapshot), so `docker stop` no longer has to kill them.
- Browser build: saves happen at most 5 seconds after a change and when the tab is hidden, the page asks before you leave with unsaved changes, and a second tab waits for the first to save before it takes over a document.
- Browser build: a page left open across a deploy says "uApp was updated" and offers to reload.
- Building needs Rust 1.88 or newer.
- The AI chat defaults to Claude Opus 5.5 and shows a summary of its thinking. There's a provider for any OpenAI-compatible server, with a context size setting.
- Manual mode asks before the assistant searches the web or fetches a URL, and each chat keeps its own mode.
- Long chats keep working. Older tool output is trimmed, the chat is summarized again whenever it outgrows its budget, and only images from the last few messages are sent again. Images over 3.75 MB or 8000 px are refused when you attach them.
- Stop interrupts a reply in progress, and the assistant says so when it stops after 40 steps.
- Provider errors worth retrying (an overloaded server, a reply cut off mid-stream) are retried with backoff.
- MCP tool lists are cached for a few minutes, so a server that's down no longer holds up every message.

### Fixed

- .uapp files could end up as zero bytes on Android after "open with". A zero-byte file uApp has opened before is now restored from its newest snapshot.
- A failed Save As or encryption change could leave the app on an empty database.
- Opening a damaged copy of an app could restore a snapshot of a different copy.
- Exports, templates and publishes failed on apps with FTS5 tables or generated columns.
- One crashing request made every later request fail too.
- The chat no longer redraws itself every few seconds, so open tool cards and text selections stay put, and Enter no longer sends while an input method is composing.
- The Linux packages now open the .uapp file you double-clicked.
- Browser build: failed saves were silent. They now show a notice with the reason, Download a copy and Try again, and closing the document waits for the save.
- Browser build: an engine crash kept auto-saving over the last good copy. It now stops and offers a reload.
- Browser build: a load failure left the splash screen up forever. It now shows the error with Try again.
- Browser build: publishing a copy that had no record of the server's version could overwrite a newer copy on the server. It now asks first.
- Browser build: an answer that arrived after its call timed out could be taken as the answer to the next call.
- Browser build: reloading an `?open=` page went to the app's own path instead of the page.

### Security

- App and assistant SQL runs in a sandbox. It refuses writes to uApp's own tables (`uapp_*` and `sqlar`), transaction control, `ATTACH`, PRAGMAs outside a read-only list (so `user_version` can't be set), and `load_extension()`. Each refusal says what was refused and why.
- Schema SQL copied from another file (templates, publishes, library uploads) must be a single `CREATE` statement, and that file is opened read-only with untrusted schema features off.
- A tool call an app triggers no longer runs when you deny it or the prompt times out.
- `browser.open` only opens http(s) and `mailto:` links.
- Apps can no longer change machine preferences, and removing a launcher only deletes launchers uApp made.
- The local server refuses requests from other origins (including other localhost ports) and can't be framed by other sites. The shell has a strict Content Security Policy.
- Session tokens come from the OS random number generator, are compared in constant time and are removed from the address bar once the cookie is set. Public sites only take the token in the URL to open the editor.
- The unlock page for encrypted files slows down repeated wrong passwords.
- `web.fetch` and `files.fetch` check every address and redirect, so a public URL can't redirect to a local service.
- Archived files are capped at 512 MiB when unpacked.
- Each WebSocket connection has a limit on requests in flight.
- Exports and library uploads strip API keys, MCP headers and approvals that older files still carry.
- Browser build: only frames inside the shell can call it, only the launcher can list or delete other stored apps, an app from another site asks before it opens, the publish token stays in memory, and downloads and publishes leave out API keys, MCP headers and approvals.
- Updated rustls for RUSTSEC-2026-0285, -0346, -0348 and -0349.

### Removed

- The `uapp-server` and `uapp-desktop` programs, and the short-lived `uapp-library`. Use `uapp`, `uapp library` and the other `uapp` commands instead.
- `make-demo-apps` is a cargo example now (`cargo run --example make-demo-apps`).
- The `uapp_applied` table from the old sync engine (dropped when a file is upgraded).

## [0.2.12] - 2026-08-30

The last release before this changelog. Its notes are on the [releases page](https://github.com/JoshTheDerf/uApp/releases/tag/v0.2.12).

[Unreleased]: https://github.com/JoshTheDerf/uApp/compare/v0.2.12...HEAD
[0.2.12]: https://github.com/JoshTheDerf/uApp/releases/tag/v0.2.12
