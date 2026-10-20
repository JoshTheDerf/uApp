# The app API (uapp.js)

`uapp.js` is the small library an app inside a `.uapp` uses to read and write its own database and files, register actions the assistant can call, and control the toolbar around it. This page is for app authors (and the assistant). The file layout it works on is in [FORMAT.md](FORMAT.md).

## Loading it

```html
<script src="/uapp.js"></script>
```

That's all you need. uApp also adds the tag to any HTML page in `app/` that doesn't load it, so the global `uapp` is always there. It works the same in the desktop and Android apps, in the browser (`uapp open --browser`) and in the browser build.

On a public site served by `uapp serve`, a page opened outside the editor has nothing to talk to, so every call rejects at once with the code `no_host`.

### Editor types

The full API with types and doc comments is served at `/uapp.d.ts` while an app is open. Save it next to your code and point a reference comment at it:

```js
/// <reference path="./uapp.d.ts" />
```

(From inside the app, `await (await fetch("/uapp.d.ts")).text()` gets you the same file.)

## Getting started

```js
const info = await uapp.ready;     // {name, user, device, appId, ...}
const { rows } = await uapp.query("SELECT * FROM jobs WHERE status = ?", ["open"]);
const job = await uapp.get("SELECT * FROM jobs WHERE id = ?", [id]);   // a row or null
const { insertId } = await uapp.exec(
  "INSERT INTO jobs(title, created) VALUES(?, ?)", [title, Date.now()]);
const stop = uapp.onChange(render);  // call stop() to unsubscribe
```

`uapp.ready` resolves once the connection is up. After that, `uapp.user` and `uapp.device` are filled in too.

## Reading and writing data

| Call | Returns |
| --- | --- |
| `uapp.query(sql, params?)` | `{columns, rows}`, with each row an object keyed by column name. |
| `uapp.get(sql, params?)` | The first row, or `null`. |
| `uapp.exec(sql, params?)` | `{changes, insertId}` (also as `insert_id`). |
| `uapp.batch(sql)` | `{ok: true}` |

- `query` and `get` only take read-only SQL. A write there fails with "use exec for writes".
- `exec` runs one statement in its own transaction. With `RETURNING` it also returns `columns` and `rows`, which is the easiest way to read back a generated value in the same statement.
- `batch` runs several statements separated by `;`, all in one transaction, so either every statement lands or none do. It takes no parameters.
- Parameters bind to `?` placeholders, and the count has to match. Strings, numbers and `null` go in as they are, booleans become 0 or 1, and other objects and arrays are stored as JSON text. To store a BLOB, pass `{b64: "..."}`. BLOBs come back the same way.
- A query stops at 50,000 rows and asks you to add a `LIMIT`.

Every write is committed straight into the `.uapp` file before the promise resolves.

## What SQL can and can't do

App SQL (and the assistant's, and the SQL panel's) runs in a sandbox. You can create, change and drop your own tables, views, indexes and triggers, and read anything. uApp refuses, with an error that says what and why:

- **Writes to uApp's own tables.** `sqlar` and every table named `uapp_*` can be read but not changed, and you can't create new tables with those names. Use the file calls below to change files.
- **Transaction control.** `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVEPOINT` aren't allowed, since every `exec` and `batch` already is a transaction.
- **`ATTACH` and `DETACH`.** Only the assistant can attach another database, read-only, after you approve it.
- **Most PRAGMAs.** Ones that only describe the schema are allowed with any argument: `table_info`, `table_xinfo`, `table_list`, `index_list`, `index_info`, `index_xinfo`, `foreign_key_list`, `foreign_key_check`, `integrity_check`, `quick_check`, `database_list`, `collation_list`, `function_list`, `module_list`, `pragma_list` and `compile_options`. These can be read but not set: `user_version`, `application_id`, `schema_version`, `data_version`, `page_count`, `page_size`, `freelist_count`, `encoding`, `foreign_keys`, `journal_mode`, `synchronous` and `auto_vacuum`. (Setting `foreign_keys` is silently ignored rather than refused.) Everything else is refused.
- **`user_version`.** It holds the file's format version now. If your app tracks its own schema version, keep it in a table of your own.
- **`load_extension()` and the `sqlcipher_*` functions.**

The connection also runs in SQLite's defensive mode, so `writable_schema` and similar tricks are off.

## Errors

Every failed call rejects with a `uapp.UappError`:

```js
try {
  await uapp.exec("INSERT INTO jobs(title) VALUES(?)", [title]);
} catch (e) {
  if (e instanceof uapp.UappError && e.code === "timeout") retryLater();
  else showError(e.message);
}
```

- `e.method` is the call that failed (`sql.exec`, `files.read`, ...).
- `e.code` says why, when it's known:
  - `timeout`: no reply within 60 seconds (set `uapp.rpcTimeout`, in milliseconds, to change it). Retrying a read is safe.
  - `disconnected`: the connection dropped mid-call. uapp.js reconnects on its own.
  - `no_host`: the page isn't running inside uApp.
  - `unknown_action`: `uapp.call` named an action this page hasn't registered.
- Errors from uApp itself (a refused statement, a missing file) only have a `message` for now, so `code` is unset.

`e.name` is `UappError`, or `UappTimeout` / `UappNoHost` for those two codes.

## Reacting to changes

| Call | Runs `cb` when |
| --- | --- |
| `uapp.onChange(cb)` | The app's table data changes, from this page, another window or the assistant. |
| `uapp.onFilesChange(cb)` | Files in the archive change. |
| `uapp.onPresent(cb)` | Someone asks every open window to show a file (`{name, mode, by}`). |
| `uapp.onTeardown(cb)` | The page is about to go away (an edit reloads it). Dispose renderers here. WebGL contexts are released for you either way. |

Each one returns a function that unsubscribes. `onChange` and `onFilesChange` callbacks get `{type: "changes", data, files, chat, config}`, with a boolean for each kind of change.

`uapp.whenVisible(el, cb)` runs `cb` once `el` is actually on screen (right away if it already is). It's a good idea to use it before you set up a grid or chart, since most of them measure themselves on init and draw nothing inside a hidden container.

## Actions

An action is a named piece of business logic. Your UI calls it, and the assistant can call the same code as the tool `app__<name>`, so it can work with the app's data the way your UI does.

```js
uapp.action("add_employee", {
  description: "Add an employee. Returns {id}.",
  params: {
    name: { type: "string" },
    hired: { type: "string", description: "YYYY-MM-DD" },
  },
}, async ({ name, hired }) => {
  const { insertId } = await uapp.exec(
    "INSERT INTO employees(name, hired) VALUES(?, ?)", [name, hired]);
  return { id: insertId };
});

await uapp.call("add_employee", { name: "Ada", hired: "2026-10-01" });
```

- `description` is what the assistant reads, so say what it does and what it returns.
- `params` describes each parameter (`type`, `description`, `enum`). They're all required unless one has `required: false`. Pass `schema` instead for a full JSON schema.
- `readonly: true` marks an action that changes nothing, so it never needs approval. Other actions may ask the user first.
- `uapp.call(name, input)` runs one of this page's own actions directly.

### Calling tools

`uapp.tool(name, input)` calls any tool the assistant has: the built-in ones, MCP tools (`mcp__<server>__<tool>`) and app actions (`app__<name>`). `uapp.tools()` lists them, with whether each one is `gated`. A gated tool shows the user an approval prompt first, and if they say no (or don't answer in time) the call fails with an error starting with `denied:` and nothing runs. A tool turned off in the app's settings fails too.

## Files

| Call | What it does |
| --- | --- |
| `uapp.readFile(name)` | Returns `{name, b64, text}`. `text` is `null` when the bytes aren't valid UTF-8. |
| `uapp.writeFile(name, text)` | Writes a text file and returns `{ok, name}` with the name it was stored under. |
| `uapp.listFiles()` | `{columns: ["name", "sz", "mtime", "role"], rows}`. Unlike `query`, the rows here are arrays. |
| `uapp.importCsv({file, table?, delimiter?, header?, replace?})` | Imports a CSV that's already in the archive into a table (created if missing). `replace: true` empties the table first. Returns `{table, rows, columns}`. |
| `uapp.fetchUrl(url, name?)` | Downloads a public http(s) URL into the archive (by default as `app/vendor/<file name>`). Local and private network addresses are refused, and so is anything over 25 MB. |
| `uapp.presentFile(name, mode?)` | Asks every open window to show a file (`auto`, `raw`, `pretty` or `edit`). |

Names live under `app/` (the app's own files) or `data/` (content the user added). A name without either goes under `app/`, except `uploads/...`, which goes under `data/`. Every write and delete is kept in the file history, so it can be reverted from the Files panel.

To write binary content, use the lower-level call: `uapp.rpc("files.write", {name, b64})`.

Writing a file doesn't reload the app. Reload the page yourself if you need to.

## The toolbar and panels

The toolbar is the bar uApp puts around the app (its name, Files, Database, Settings, chat). Hidden, the app fills the window.

- `uapp.toolbar.hide()`, `.show()` and `.toggle()` change this window right now and save nothing. They return `false` when no uApp window is hosting the page.
- `await uapp.toolbar.state()` returns `{visible, hidden, shortcut, panel}`, or `null` outside uApp.
- `await uapp.toolbar.setDefault({hidden, shortcut})` changes how the app opens, and is saved in the file. `shortcut` is a key like `"F9"` or `"Mod+Alt+B"` (`Mod` is Cmd on macOS and Ctrl elsewhere), and `""` removes it. A plain letter needs a modifier.

The panels beside the app are `"chat"`, `"files"`, `"database"`, `"settings"` and `"tools"`:

- `uapp.panel.open(name)`, `.close(name)` and `.toggle(name)`. Opening one closes whichever was open and shows the toolbar if it was hidden.
- `await uapp.panel.state()` returns the open panel's name, or `null`.

None of this is saved, apart from `setDefault`.

## Lower-level calls

`uapp.rpc(method, params)` sends any method to uApp and is what the calls above are built on. Stick to the documented calls where you can. App code currently runs on the same origin as the editor (see [design/app-isolation.md](design/app-isolation.md)), and once it's isolated, app pages will only get the methods the calls on this page use.
