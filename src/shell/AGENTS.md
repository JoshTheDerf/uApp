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

## Rules

- Vendor dependencies into `app/vendor/`.
- Keep user content in `data/`.
