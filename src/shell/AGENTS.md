# Editing this uApp

This file is a uApp: app + data + AI history in one `.uapp` file (a SQLite database).

## Layout

- `app/`: app code. `index.html` is the entry point.
- `data/`: user content.
- SQL tables: the data model. `sqlar` and the `uapp_*` tables belong to uApp.

## Editing outside the runtime

Close any running instance first. The runtime writes directly to the file.

```sh
sqlite3 -A -f app.uapp --extract    # extract
sqlite3 -A -f app.uapp --update     # update
```

Or write the `sqlar` table directly: store bytes with `sz = length(data)`.
Keep every name under `app/` or `data/`.

Then open with `uapp open <file>.uapp` to test.

## API

```html
<script src="/uapp.js"></script>
```

- `await uapp.query(sql, params)`: read. `{columns, rows}` with rows as objects
- `await uapp.get(sql, params)`: the first row, or `null`
- `await uapp.exec(sql, params)`: one write in its own transaction.
  `{changes, insertId}`, plus `rows` with `RETURNING`
- `await uapp.batch(sql)`: several statements in one transaction, no params
- `uapp.action(name, {description, params, readonly}, handler)`: register
  business logic. UI calls it with `uapp.call(name, input)`, the assistant as
  the tool `app__<name>`
- `uapp.onChange(cb)`: re-render on changes. Returns a function that
  unsubscribes (so do `onFilesChange`, `onPresent`, `onTeardown`)
- `uapp.readFile(name)`, `uapp.writeFile(name, text)`, `uapp.listFiles()`
- `await uapp.ready`: `{name, user, device, appId, ...}`

Failed calls reject with `uapp.UappError`. `.method` is the call, `.code` is
`"timeout"`, `"disconnected"`, `"no_host"` or `"unknown_action"` when known.

SQL runs in a sandbox. It refuses writes to `sqlar` and `uapp_*`, BEGIN,
COMMIT, ROLLBACK and SAVEPOINT (each exec or batch is already a
transaction), ATTACH, and most PRAGMAs (read-only ones like `table_info`
work). `user_version` is uApp's, so keep your own schema version in a table.

The full API with types and doc comments is `/uapp.d.ts` (read it with
`fetch("/uapp.d.ts")`, or reference it from an editor).

### The toolbar

The bar uApp puts around this app (its name, Files, Database, Settings, chat).
Hidden, the app fills the window.

- `uapp.toolbar.hide()` / `.show()` / `.toggle()`: this window, right now.
  Saves nothing, so the app still opens the way its setting says.
- `await uapp.toolbar.state()`: `{visible, hidden, shortcut, panel}`
- `await uapp.toolbar.setDefault({hidden, shortcut})`: how the app *opens*,
  saved in the file. `shortcut` is a keystroke like `"F9"` or `"Mod+Alt+B"`
  (`Mod` = Cmd on macOS, Ctrl elsewhere).

The panels beside the app are `"chat"`, `"files"`, `"database"`, `"settings"`
and `"tools"`:

- `uapp.panel.open(name)` / `.close(name)` / `.toggle(name)`
- `await uapp.panel.state()`: the open panel's name, or `null`

Opening one closes whichever was open (they share an edge) and reveals the
toolbar if it was hidden.

## Rules

- Vendor dependencies into `app/vendor/`.
- Keep user content in `data/`.
