# Tools

The AI chat (and apps themselves via `uapp.tool()`) have access to a set of built-in tools that operate on the .uapp's database and file archive. Tools that mutate state are **gated** and require user approval when the chat is in manual mode.

## Approval modes

- **Auto** (the default): gated tools run without prompting, except for operations that reach outside the app (ATTACH'ing another database, contacting local/private network URLs).
- **Manual**: every gated tool prompts for approval before running, and so do `fetch_url` and `web_search` (a page the assistant reads could tell it to send your data somewhere).
- **Plan**: read-only. Gated tools, sub-agents and MCP tools are left out, and the assistant proposes a plan instead of making changes.

You can switch modes in the chat header. Each chat keeps its own mode, so switching one chat to Auto doesn't stop the prompts in another.

## Built-in tools

### Database

| Tool | Gated | Description |
|------|-------|-------------|
| `get_schema` | No | Get all table definitions and row counts in the app database. |
| `sql_query` | No | Run read-only SQL (SELECT/WITH/PRAGMA). Complex queries, joins and aggregates are encouraged. |
| `sql_exec` | Yes | Run ONE deterministic write statement (INSERT/UPDATE/DELETE/DDL) with optional params. |
| `sql_batch` | Yes | Run multiple write statements separated by semicolons as one atomic write. |
| `import_csv` | Yes | Parse a CSV/TSV file from the archive into a database table as one efficient bulk write. Auto-detects delimiter and column types. |

### Files

| Tool | Gated | Description |
|------|-------|-------------|
| `list_files` | No | List files stored in the app archive. |
| `read_file` | No | Read a file from the archive (returns text when valid UTF-8). |
| `write_file` | Yes | Create or overwrite a text file in the archive. Previous version is kept in history. The app iframe reloads automatically. |
| `edit_file` | Yes | Edit a text file by exact string replacement — prefer this over `write_file` for changes to existing files. `old_string` must match byte-for-byte and appear exactly once unless `replace_all` is set. |
| `delete_file` | Yes | Delete a file from the archive. |
| `present_file` | No | Show a file to the user in their file viewer (images, video, audio, PDF, markdown rendered, code with highlighting). |
| `download_lib` | Yes | Vendor a JS/CSS/WASM library or asset from a URL into the archive. Stored under `app/vendor/` by default. |

### JavaScript

| Tool | Gated | Description |
|------|-------|-------------|
| `run_js` | Yes | Execute JavaScript in the browser. Context `scratchpad` (default): a hidden empty page with the full uapp API and a `loadScript(url)` helper — globals persist between calls. Context `app`: runs inside the LIVE app page with its DOM and globals. |
| `read_console` | No | Read recent console output and uncaught errors (with stack traces) from the LIVE app page. Use this to debug after edits or UI-driving `run_js` calls. |

### The app window

| Tool | Gated | Description |
|------|-------|-------------|
| `show_toolbar` | No | Show or hide the uApp toolbar — the bar around the app with its name, Files, Database, Tools, Settings and chat. Hidden, the app fills the window on its own. This session only. |
| `set_toolbar_default` | Yes | Change how the app *opens*: whether the toolbar starts hidden, and the keystroke that toggles it. Saved in the .uapp, so it travels with the app. |
| `show_panel` | No | Open or close one panel beside the app: `chat`, `files`, `database`, `settings`, `tools`. Put the user where the answer is instead of describing where to click. |

`show_toolbar` deliberately saves nothing: someone who reveals a hidden
toolbar to change one thing has not decided the app should look different from
then on. `set_toolbar_default` is the decision. Hiding it never traps anyone —
a reveal handle appears in the corner (the only way back on a phone) and the
shortcut keeps working, including while focus is inside the app.

The panels share one edge of the window, so `show_panel` opening one closes
whichever was open, and it reveals the toolbar if it was hidden (the panels
hang off it). `sql` is accepted for `database`, `ai` for `chat`.

### Delegation & user interaction

| Tool | Gated | Description |
|------|-------|-------------|
| `agent_run` | No | Delegate a self-contained task to an autonomous sub-agent. It works in its own conversation and returns a written report. |
| `agent_send` | No | Send a follow-up instruction to a sub-agent started with `agent_run`. |
| `ask_user` | No | Ask the user 1-4 questions and wait for their answers. Each question always offers a free-form 'Other' field. Use it when a decision genuinely belongs to the user. |
| `call_action` | Like the action | Call one of the app's actions by name (see [App actions](#app-actions)). |

### Web

| Tool | Gated | Description |
|------|-------|-------------|
| `web_search` | Manual mode | Search the web via DuckDuckGo (no API key). Returns titles, URLs and snippets. |
| `fetch_url` | Manual mode | Fetch a web page and return its readable text (HTML stripped, 20KB cap). |

### MCP (Model Context Protocol) servers

| Tool | Gated | Description |
|------|-------|-------------|
| `add_mcp_server` | Yes | Connect a remote MCP server. Its tools appear immediately as `mcp__<name>__*`. |
| `remove_mcp_server` | Yes | Disconnect a previously added MCP server. |

MCP tools are **always gated** (unknown side effects) and are prefixed with `mcp__`. Providers only accept tool names made of letters, digits, `_` and `-`, up to 64 characters, so a longer or odd MCP tool name is shortened for the model (with a short hash to keep it unique). Apps still call it by its full `mcp__<server>__<tool>` name.

## App actions

Apps can expose their own business logic as named actions via `uapp.action()`:

```js
uapp.action("add_employee", {
  description: "Add an employee. hired is YYYY-MM-DD.",
  params: { name: {type: "string"}, hired: {type: "string"} },
}, async ({name, hired}) => {
  await uapp.exec("INSERT INTO employees(name, hired) VALUES(?,?)", [name, hired]);
  return {ok: true};
});
```

Actions automatically become tools available to the AI chat (named `app__<name>`), so the human clicking a button and the AI answering a chat request run the **same code**. Actions run inside the open app page and exist as tools only while the app is open.

The assistant's tool list is fixed when it starts answering a message (changing it mid-answer would throw away the prompt cache), so an action registered after that, such as one it just wrote, is called through `call_action {"action": "name", "input": {...}}`. Called with an unknown name, `call_action` returns the registered actions and their parameters.

Mark pure reads with `readonly: true` to skip approval prompts.

## Forced gates (even in auto mode)

Some operations always require approval, even in auto mode:

- **SQL with ATTACH** — attaching another database file on the machine
- **Local/private network URLs** — `fetch_url` or `download_lib` to non-public addresses

These protect against accidentally reaching outside the .uapp's sandbox.

## App-triggered tools

When an app calls a gated tool via `uapp.tool()`, `uapp.exec()`, or through an action handler, it prompts for user approval as if the chat were in manual mode. The approval prompt shows which app is requesting access and which tool it wants to call.

The user can choose:
- **Allow once** — run this call, prompt again next time
- **Always allow** — store approval for this app+tool combination, skip future prompts
- **Deny** — don't run, return error to app

"Always allow" is remembered on this computer (not in the .uapp file, so it doesn't travel with a copy of the app) and persists across restarts. It's scoped per app, so approving a tool for one app doesn't approve it for others.
