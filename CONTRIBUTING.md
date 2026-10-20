# Contributing

Thanks for taking a look. Bug reports and pull requests are welcome on [GitHub](https://github.com/JoshTheDerf/uApp). For anything security-related, see [SECURITY.md](SECURITY.md) instead.

## Building and testing

[BUILDING.md](BUILDING.md) has the prerequisites and every build. Day to day, you'll mostly want [`just`](https://github.com/casey/just):

```sh
just test     # Rust tests, then the node end-to-end suites
just lint     # cargo fmt --check, and clippy (-D warnings) for both builds
```

`just test` runs `cargo test --no-default-features` and then `scripts/test-e2e.sh`, which builds `uapp` without the window and runs every `tests/test_*.mjs` suite against it, each with its own scratch `HOME` (so your real config is never touched). The suites need Node.js 22 and the `sqlite3` CLI. To run only some, name them:

```sh
scripts/test-e2e.sh ai approval    # tests/test_ai.mjs and tests/test_approval.mjs
```

The browser build has its own end-to-end run in headless Chromium. It isn't part of `just test`, since it needs the wasm build:

```sh
cargo build --no-default-features
scripts/build-web.sh
node scripts/e2e-webhost/run.mjs
```

CI runs all of the above except the browser e2e, plus a wasm32 check, cargo-deny and a Windows and macOS `cargo check`. It's a good idea to run `just lint` and `just test` before you open a pull request.

## Commit messages

One sentence in the summary line, saying what the change does, ending with a period. Then a blank line and a short body, wrapped at about 72 columns, that says why and anything a reviewer should know. For example:

```
Stop .uapp files ending up as zero bytes on Android.

Write-back to a linked original opened it with "wt", which truncates
before writing, and an "open with" relaunch could exit mid-write.
```

Code comments and docs follow the same plain style.

## Where things live

| Path | What's there |
| --- | --- |
| `src/main.rs` | The `uapp` binary: the CLI (clap) and opening the window. |
| `src/lib.rs`, `src/app.rs` | Opening a file and the shared state of one open app. |
| `src/store.rs` | The file format: schema, migrations, the SQL sandbox, exports. See [docs/FORMAT.md](docs/FORMAT.md). |
| `src/engine.rs` | The SQLite connection, writes, snapshots, Save As, encryption changes. |
| `src/cipher.rs` | SQLCipher encryption. |
| `src/template.rs` | Updating an app from a template, and publishing a site. |
| `src/server.rs`, `src/rpc.rs` | The local HTTP and WebSocket server, and every RPC method. |
| `src/secrets.rs`, `src/registry.rs`, `src/prefs.rs` | What's kept on this machine: secrets, open-file locks and snapshot folders, preferences. |
| `src/webhost.rs`, `src/library.rs` | `uapp serve` without a file, and `uapp library`. |
| `src/ai.rs`, `src/tools.rs`, `src/mcp.rs` | The AI chat, its tools and the MCP client. |
| `src/net.rs` | Outgoing HTTP, with the checks that keep fetches off local addresses. |
| `src/gui.rs`, `src/native.rs`, `src/permissions.rs` | The Tauri window (the `gui` feature), and camera and microphone prompts. |
| `src/install.rs`, `src/shortcut.rs` | `uapp install`, and launcher entries for single apps. |
| `src/toolbar.rs`, `src/logging.rs` | The toolbar's saved default, and logging. |
| `src/wasm.rs` | The browser build's entry point. |
| `src/shell/` | The editor UI around the app, `uapp.js` (the [app API](docs/APP-API.md)), `uapp.d.ts`, and the `AGENTS.md` new apps start with. |
| `web/` | The browser build's page, workers and boot modules. `scripts/build-web.sh` turns it into `dist-web/`. |
| `tests/` | Rust integration tests (`*.rs`) and the node suites (`test_*.mjs`). |
| `examples/` | Demo apps, and `make-demo-apps.rs`, which builds the browser build's launcher from `templates/`. |
| `gen/android/` | The Android project. |
| `docs/design/` | Design notes, such as the app isolation plan. |
