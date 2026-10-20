# The .uapp file format

This is the layout of a `.uapp` file, for anyone writing a tool that reads or writes one. It describes format 1, the current one. Everything here comes from `src/store.rs` unless it says otherwise.

A `.uapp` is a SQLite 3 database. It holds the app's files in an [sqlar](https://sqlite.org/sqlar.html) table, a few housekeeping tables whose names start with `uapp_`, and whatever tables the app itself creates. It may be encrypted with SQLCipher (see [Encryption](#encryption)).

## Telling a .uapp apart

Two fields in the SQLite header say what the file is:

| Header field | Value | Meaning |
| --- | --- | --- |
| `application_id` | `0x75417070` (`"uApp"` in ASCII) | This SQLite file is a .uapp. |
| `user_version` | `1` | The format version (`FORMAT_VERSION`). |

`uapp_meta` repeats the version under the key `format_version`, for readers that only look there.

When uApp opens a file, it first runs `PRAGMA quick_check(1)` and refuses a file that fails it. Then it reads the header:

- **Another program's file.** An `application_id` that's neither 0 nor `0x75417070` is refused ("a SQLite database belonging to another program").
- **A newer file.** If the version is higher than the one this build knows, the file is refused with "made by a newer uApp" and left alone. For a stamped file the version is the higher of `user_version` and `uapp_meta.format_version`.
- **An older or unstamped file.** A file with `application_id` 0 is read as format 0 (everything from before format versioning), going by `uapp_meta.format_version` if it has one. uApp runs the migrations it needs inside one transaction, so the file is upgraded completely or not at all, then stamps the header. If an unstamped file had its own `user_version` (apps could set it back then), that number is kept in `uapp_meta.app_user_version`.
- **An up-to-date file** is opened without writing anything.

If you write a .uapp from scratch, set both header fields and `uapp_meta.format_version`, or leave all three unset and let uApp upgrade the file on first open.

Format history:

| Version | Changes |
| --- | --- |
| 0 | Unstamped files from before format versioning (any earlier layout). |
| 1 | `app/` and `data/` archive roots, `uapp_chat.session`, `uapp_file_history.sz`, no `uapp_applied` table. |

## Tables

```sql
CREATE TABLE sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
CREATE TABLE uapp_meta(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE uapp_config(key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE uapp_chat(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  mid TEXT UNIQUE, ts INTEGER, ctr INTEGER, device TEXT, user TEXT,
  role TEXT, content TEXT, session TEXT);
CREATE TABLE uapp_sessions(
  id TEXT PRIMARY KEY, title TEXT, created INTEGER,
  kind TEXT DEFAULT 'chat', parent TEXT, archived INTEGER DEFAULT 0);
CREATE TABLE uapp_file_history(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT, ts INTEGER, device TEXT, user TEXT, action TEXT, data BLOB, sz INT);
```

uApp creates any of these that are missing every time it opens a file, so a file with only `sqlar` (or an empty file) opens fine. Every other table, view, index and trigger belongs to the app. The names `sqlar` and anything starting with `uapp_` are reserved, and app SQL can't create or change them (see [docs/APP-API.md](APP-API.md#what-sql-can-and-cant-do)).

Timestamps are milliseconds since the Unix epoch, except `sqlar.mtime`, which is seconds (as in the sqlar format).

### sqlar: the app's files

The standard sqlar layout, so `sqlite3 -A` can list, extract and update it.

| Column | What it holds |
| --- | --- |
| `name` | The path, always under `app/` or `data/` (see below). |
| `mode` | Unix mode. uApp writes `420` (0644). |
| `mtime` | Modification time in seconds. |
| `sz` | The file's original size in bytes. |
| `data` | The content, zlib-compressed when that's smaller. `NULL` for a directory entry. |

Compression follows `sqlar_compress()`: if `length(data) == sz` (or `sz <= 0`) the blob is the content as is, and if `length(data) < sz` it's zlib (deflate level 6 when uApp writes it). uApp refuses to inflate any file past 512 MiB, whatever `sz` claims.

The archive has two top-level folders, and a file's folder is its role:

- **`app/`** is the app itself, with `app/index.html` as the entry point. It's the app's web root (a root-absolute link like `/js/main.js` finds `app/js/main.js`). These files ship in templates and are the only ones a public site serves.
- **`data/`** is content the user put in (uploads, imported CSVs). It stays out of templates and isn't served publicly.

A name outside both folders is moved in when the file is opened: `uploads/x` goes to `data/x`, and anything else (`index.html`, `/js/app.js`) goes under `app/`. So a tool that writes plain names still works, but it's better to write the full `app/...` or `data/...` name yourself.

A brand-new app gets `app/index.html` (a starter page) and `app/AGENTS.md` (a short guide for coding agents that edit the file from outside uApp).

### uapp_meta: identity

| Key | Value |
| --- | --- |
| `app_id` | The app's id, 16 random letters and digits for new apps. uApp only uses an id matching `[A-Za-z0-9_-]{1,64}` and treats anything else as `unknown`. Copies made with Save As or `uapp export` keep it, templates get a new one. |
| `name` | The display name. |
| `created` | When the app was created (ms). |
| `format_version` | The format version, same as `user_version`. |
| `app_user_version` | Only on files upgraded from format 0 that had set `user_version` themselves. |
| `template_source`, `template_source_name`, `template_updated` | The `app_id` and name of the template the app was last updated from, and when (ms). |

### uapp_config: settings that travel with the file

Each value is JSON text. The keys uApp uses:

| Key | Value |
| --- | --- |
| `toolbar` | `{"hidden": bool, "shortcut": "F9"}`, how the app opens and the key that toggles the toolbar. |
| `tools_disabled` | An array of tool names turned off for this app. |
| `ai` | The assistant's provider settings (provider, model, base URL and so on). Never the API key on desktop and Android. |
| `mcp` | An array of MCP servers. Never their headers on desktop and Android. |
| `approvals.app.<app_id>` | Browser build only: tools the user chose "always allow" for. Desktop and Android ignore these. |
| `file_roles` | Legacy. Read once to sort old files into `app/` and `data/`, then deleted. |

The app's own settings belong in its own tables. App code can read `uapp_config` but can't write to it with SQL.

### uapp_chat and uapp_sessions: the assistant's conversations

`uapp_chat` has one row per message. `mid` is a unique message id, `session` names the conversation (`NULL` means `main`), `role` is who wrote it and `content` is JSON. `ctr` is left over from an older sync engine and is always 0 now. Rows are in `id` order.

`uapp_sessions` has one row per conversation besides `main`. `kind` is `chat` for a chat tab or `agent` for a sub-agent, whose `parent` is the session that started it.

The message content is uApp's own and may change between versions, so treat it as opaque unless you're displaying it.

### uapp_file_history: earlier versions of files

Before uApp writes or deletes a file in `sqlar`, it copies the old row here: the file's `name`, when (`ts`), who (`device`, `user`), `action` (`put` or `del`), and the old content in `data`/`sz` with the same compression rules as `sqlar`. A `NULL` `sz` means an old row stored uncompressed. uApp keeps the 40 newest entries per file.

## What never goes in the file

These belong to the person at the machine, so the desktop and Android apps keep them outside the `.uapp`:

| What | Where |
| --- | --- |
| The AI API key, MCP server headers, "always allow" answers | `<config dir>/uapp/secrets/<path key>.json` (owner-only) |
| Machine preferences (window or browser) | `<config dir>/uapp/prefs.json` |
| This device's id | `<config dir>/uapp/device` |
| Snapshots | `<local data dir>/uapp/snapshots/<path key>/` |
| Which files are open, and by which process | `<local data dir>/uapp/servers/` |

`<config dir>` is `~/.config` on Linux, `~/Library/Application Support` on macOS and `%APPDATA%` on Windows. `<local data dir>` is `~/.local/share`, `~/Library/Application Support` and `%LOCALAPPDATA%`. On Android both are the app's private data folder.

`<path key>` is a 64-bit FNV-1a hash of the file's canonical path, as 16 hex digits (`registry::key`). Everything is keyed by the path rather than the `app_id`, so a copy of a file somewhere else starts with no secrets and no snapshots, whatever its `app_id` says.

The API key is saved together with the provider and base URL it was entered for, and MCP headers with their server's URL. If the file's `ai` or `mcp` config later names a different endpoint, the credential isn't sent and has to be entered again.

Files from older versions carried these values in `uapp_config`. On open, uApp moves an API key and MCP headers out of the file onto the machine, and ignores any `approvals.*` keys.

The browser build has no machine-local storage, so it keeps all of this in the database it stores in the browser, and strips it from every copy that leaves (see below).

## Writing safely

uApp opens files with `journal_mode=DELETE` and `synchronous=FULL`. Each write is one transaction committed straight into the file. While it commits, SQLite keeps a rollback journal next to the file, `<name>.uapp-journal`, and deletes it when the commit is done. If the process dies mid-commit, the journal stays, and the next open of the file uses it to roll the half-written transaction back.

So if you work with .uapp files from your own tools:

- Close the app in uApp first. uApp holds the file open and doesn't expect anyone else to write to it.
- A leftover journal belongs to that exact file. Copy or move it together with the file, and delete one that's left at a path before you put a different file there, or SQLite will "roll back" your new file using the old one's pages.
- Don't switch a file to WAL mode. uApp sets the journal mode back to `DELETE` when it opens the file.

## Snapshots

The desktop and Android apps keep up to 10 rolling snapshots of each file in `<local data dir>/uapp/snapshots/<path key>/`, named `<ms>.uapp`. A `source` file in the same folder names the path they were taken from, and uApp checks it before restoring anything. (Files last opened by an older build may still have snapshots in `<local data dir>/uapp/<app_id>/snapshots/`, which are used as a fallback.)

A snapshot is taken when a file is opened (unless it hasn't changed since the newest one), every 10 minutes while writes happen, when the app closes after writes, before an encryption change and after compacting. Snapshots of an encrypted file are encrypted with the same password.

If a file fails to open because it's damaged, or a file uApp has opened before is suddenly zero bytes, uApp moves it aside as `<name>.uapp.corrupt-<ms>` (with its journal, if any) and puts the newest snapshot in its place. (For an encrypted file this only happens when the password works but the integrity check fails, since a damaged header looks the same as a wrong password.) A file refused for being newer or from another program is never replaced.

The browser build doesn't take snapshots.

## Copies and exports

| Copy | How you get one | What's in it |
| --- | --- | --- |
| Full copy | `uapp export`, Save As, Download a copy | Everything, byte for byte. An encrypted app stays encrypted. |
| Template | `uapp export --template`, Download template | `app/` files, the app's tables, views, indexes and triggers with no rows, and `uapp_config` minus secrets. |
| Public copy | `/site.uapp` from `uapp serve`, files stored by `uapp library` | Like a template, plus every table's rows. `data/` only with `uapp serve --publish-data`, and always for `uapp library`. |

Templates and public copies get a new `app_id` and a fresh `uapp_meta` (name, created, format). They never carry chat, sessions or file history. From `uapp_config` they drop the `approvals.*` keys, `api_key` from `ai` and `headers` from each `mcp` server.

In the browser build, a full copy that leaves the browser (Download a copy, `/download.uapp`, publishing) has the same secrets stripped, then gets a `VACUUM` so the old values aren't left in free pages. The copy saved in the browser keeps them.

When uApp copies the app's schema out of another file (applying a template, a publish, a library upload), it opens that file read-only with `trusted_schema=OFF` and SQLite's defensive mode, and each object's SQL in `sqlite_master` has to be a single `CREATE` of the kind it claims to be. A file whose schema SQL holds anything more is refused as tampered with. Shadow tables of virtual tables (FTS3/4/5, R*Tree) aren't copied, since they come back when the virtual table is created.

## Updating from a template

`uapp update` (or dropping a `.uapp` on an open window) takes the code from a template and keeps the data:

- `app/` files are added and replaced from the template, and ones it no longer has are removed (unless `--keep-stale`). Each write goes into `uapp_file_history`, so it can be reverted.
- Missing tables are created and missing columns added. Nothing is dropped or retyped. Views, indexes and triggers whose definition differs are replaced. A change that can't be made this way (a new primary key, a changed column type) shows up as a warning.
- `uapp_config` keys the app doesn't have yet are copied in (without an `api_key`). Existing keys are left alone.
- `data/`, rows, chat, file history, the `app_id` and the name stay as they were. The template's id and name are recorded in `uapp_meta` (`template_source`, `template_source_name`, `template_updated`).

A snapshot is taken first.

## Encryption

An encrypted .uapp is a SQLCipher 4 database with the default settings (AES-256 with a per-page HMAC, PBKDF2-HMAC-SHA512 with 256,000 iterations and a random salt per file). Its header is encrypted too, so `application_id` can't be read without the password. uApp treats a non-empty file that doesn't start with `SQLite format 3` as encrypted and asks for the password. There's no recovery if it's lost.

`uapp encrypt`, `decrypt` and `passwd` keep the `application_id` and `user_version`. The browser build can't open encrypted files.
