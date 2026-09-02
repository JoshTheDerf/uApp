# Editing this uApp

This file is a uApp: app + data + AI history in one `.uapp` file.

## Layout

- `app/` — app code. `index.html` is the entry point.
- `data/` — user content.
- SQL tables — the data model.

## Editing outside the runtime

Close any running instance first. The runtime writes directly to the file.

```sh
sqlite3 -A -f app.uapp --extract    # extract
sqlite3 -A -f app.uapp --update     # update
```

Or write the `sqlar` table directly: store bytes with `sz = length(data)`.

Then open with `uapp open <file>.uapp` to test.

## API

```html
<script src="/uapp.js"></script>
```

- `await uapp.query(sql, params)` — read
- `await uapp.exec(sql, params)` — write
- `uapp.action(name, {description, params}, handler)` — register business logic
- `uapp.onChange(cb)` — re-render on changes
- `await uapp.ready` — `{user, device, …}`

### The toolbar

The bar uapp puts around this app (its name, Files, Database, Settings, chat).
Hidden, the app fills the window.

- `uapp.toolbar.hide()` / `.show()` / `.toggle()` — this window, right now.
  Saves nothing: the app still opens the way its setting says.
- `await uapp.toolbar.state()` — `{visible, hidden, shortcut}`
- `await uapp.toolbar.setDefault({hidden, shortcut})` — how the app *opens*,
  saved in the file. `shortcut` is a keystroke like `"F9"` or `"Mod+Alt+B"`
  (`Mod` = Cmd on macOS, Ctrl elsewhere).

The panels beside the app — `"chat"`, `"files"`, `"database"`, `"settings"`,
`"tools"`:

- `uapp.panel.open(name)` / `.close(name)` / `.toggle(name)`
- `await uapp.panel.state()` — the open panel's name, or `null`

Opening one closes whichever was open (they share an edge) and reveals the
toolbar if it was hidden.

## Rules

- Vendor dependencies into `app/vendor/`.
- Keep user content in `data/`.
