# uapp command line

`uapp` is one program. Run it with no command to open your scratch app, or give it a file (`uapp notes.uapp`) to open that app. Everything else is a subcommand.

The build matters for opening apps. The release builds (and a plain `cargo build`) open them in the UApp window. A build made with `--no-default-features`, which is what the Docker image and the test suites use, opens them in your system browser instead. The subcommands work the same in both.

Exit codes: `0` for success (and for `--help`), `1` when the command ran and failed, `2` for a usage error (an unknown command or option, a missing argument). Results go to stdout, and diagnostics and errors go to stderr.

## Quick examples

```sh
uapp                                  # open your scratch app
uapp notes.uapp                       # open an app (same as `uapp open notes.uapp`)
uapp open notes.uapp --browser        # in the browser, even in the window build
uapp open notes.uapp --headless       # serve it and print {"url": ...} as JSON
uapp new crm.uapp                     # create a blank app
uapp info crm.uapp --json             # name, id, format, tables, files
uapp export crm.uapp -o copy.uapp     # a full copy (encrypted if the app is)
uapp export crm.uapp -o tpl.uapp --template
uapp update crm.uapp crm-v2.uapp      # take crm-v2's code, keep crm's data
uapp serve site.uapp --chrome dist-web --bind 0.0.0.0
uapp library ./apps --port 8090
```

## Master passwords

Commands that open an encrypted app look for its password in this order:

1. `--password PW` (other users can see it in `ps`, so prefer the others)
2. `--password-stdin`, the first line of stdin
3. `--password-file PATH`, the first line of a file
4. the `UAPP_PASSWORD` environment variable
5. a hidden prompt, when stdin is a terminal

`uapp passwd` takes the new password from `UAPP_NEW_PASSWORD`, else from the second line of stdin (with `--password-stdin`), else from a prompt. (Older scripts that pass the old password as a flag and the new one in `UAPP_PASSWORD` still work.)

Opening an encrypted app in the window or the browser without a password shows an unlock page. `--headless` can't show one, so it needs the password up front.

## Environment variables

| Variable | What it does |
| --- | --- |
| `UAPP_LOG` | Log filter, in `tracing` EnvFilter syntax: `debug`, `uapp=trace`, `uapp::server=debug,warn`. Overrides `-v`/`-q`. The default is `warn` for other crates and `info` for uapp. |
| `UAPP_PASSWORD` | Master password of an encrypted app, used when no password option is given. |
| `UAPP_NEW_PASSWORD` | The new password for `uapp passwd`. |
| `UAPP_TOKEN` | The token for `uapp serve` and `uapp library` (same as `--token`). Unset, a random one is printed at start. |
| `UAPP_LINGER_SECS` | How long a server opened by `uapp open` keeps running after its last tab closes (default 90). `never` or `off` keeps it running. |
| `UAPP_DEVICE` | Overrides this machine's device id (handy for testing several "devices" on one box). |
| `UAPP_AI_TIMEOUT_SECS` | Timeout for one AI request (default 300, or 3600 for a local model). |
| `UAPP_ASK_TIMEOUT_MS` | How long the assistant waits for an answer to a question it asked you (default 300000). |
| `UAPP_ACTION_TIMEOUT_MS` | How long an app action called by the assistant may take (default 60000). |
| `UAPP_EVAL_TIMEOUT_MS` | How long code the assistant runs in a page may take (default 60000). |
| `UAPP_ALLOW_LOCAL_FETCH` | Set to `1` to let fetches reach local and private network addresses without asking (tests, intranets). |
| `UAPP_SEARCH_URL` | Web search endpoint template, `{}` is the query (default DuckDuckGo's HTML search). For tests. |
| `UAPP_SAMPLES_BASE` | Where the demo launcher looks for sample apps (`cargo run --example make-demo-apps`; default `examples/`). |

## Logs

Diagnostics go to stderr. The window build also writes them to `uapp.log` in the per-user data dir (`~/.local/share/uapp/logs/` on Linux, `%LOCALAPPDATA%\uapp\logs\` on Windows, `~/Library/Application Support/uapp/logs/` on macOS), along with any crash. The file starts over at 4 MB and the last two are kept as `uapp.log.1` and `uapp.log.2`.

## Shell completions

```sh
uapp completions bash > ~/.local/share/bash-completion/completions/uapp
uapp completions zsh > ~/.zfunc/_uapp      # with ~/.zfunc on your fpath
uapp completions fish > ~/.config/fish/completions/uapp.fish
```

## Reference

The blocks below are the output of `uapp --help` and `uapp <command> --help`.

### uapp

```text
Single-file shareable apps: app + data + AI in one .uapp file

Usage: uapp [OPTIONS] [COMMAND]

Commands:
  open         Open an app (creates it if the file is empty or missing)
  new          Create a blank app without opening it
  update       Update an app's code from a newer copy of it, keeping its data
  export       Write a copy of an app to a new file
  info         Show what's in an app file (name, id, format, tables, files)
  encrypt      Encrypt an app with a master password
  decrypt      Remove an app's encryption
  passwd       Change an app's master password
  serve        Serve an app as a public website, or the browser build on its own
  library      Serve a folder of .uapp files over HTTP (list, fetch, publish)
  install      Register .uapp files with your desktop and file manager (Linux)
  uninstall    Remove what `uapp install` set up
  completions  Print a shell completion script
  help         Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)

Options for opening the scratch app:
      --headless              Don't show any UI; print the URL as a JSON line
                              and keep serving
      --browser               Open in the system browser instead of the UApp
                              window
      --port <N>              Serve on this port (default: any free one)
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file

With no command, `uapp` opens your scratch app. `uapp FILE.uapp` is short for
`uapp open FILE.uapp`.
```

### uapp open

```text
Open an app (creates it if the file is empty or missing)

Usage: uapp open [OPTIONS] [FILE]

Arguments:
  [FILE]  The .uapp file (none: your scratch app)

Options:
      --headless              Don't show any UI; print the URL as a JSON line
                              and keep serving
      --browser               Open in the system browser instead of the UApp
                              window
      --port <N>              Serve on this port (default: any free one)
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file
  -h, --help                  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp new

```text
Create a blank app without opening it

Usage: uapp new [OPTIONS] <FILE>

Arguments:
  <FILE>  Where to create it

Options:
      --encrypt               Create it encrypted (asks for a master password)
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file
  -h, --help                  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp update

```text
Update an app's code from a newer copy of it, keeping its data.

The app's files under app/ and its schema come from TEMPLATE; rows, uploads,
chat and the app's identity stay as they are. A snapshot is taken first, so the
update can be undone. This is what dropping a .uapp onto an open window does.

Usage: uapp update [OPTIONS] <APP> <TEMPLATE>

Arguments:
  <APP>
          The app to update

  <TEMPLATE>
          The .uapp to take the code from

Options:
      --dry-run
          Print what would change, as JSON, and change nothing

      --keep-stale
          Keep app files the template no longer has (removed by default)

      --template-password <PW>
          Master password of an encrypted TEMPLATE (visible to other users in
          `ps`)

      --template-password-file <PATH>
          Read the TEMPLATE's master password from the first line of a file

      --password <PW>
          Master password (visible to other users in `ps`; prefer
          --password-stdin, --password-file or UAPP_PASSWORD)

      --password-stdin
          Read the master password from the first line of stdin

      --password-file <PATH>
          Read the master password from the first line of a file

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -v, --verbose...
          Log more (-v debug, -vv trace). UAPP_LOG overrides both flags

  -q, --quiet...
          Log less (-q warnings only, -qq errors only)
```

### uapp export

```text
Write a copy of an app to a new file

Usage: uapp export [OPTIONS] --output <OUT> <APP>

Arguments:
  <APP>  The app to copy

Options:
  -o, --output <OUT>          Where to write the copy
      --template              Export a template instead: the app's code and
                              empty tables, without rows, data/, chat, history
                              or saved keys
  -f, --force                 Replace OUT if it exists
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file
  -h, --help                  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp info

```text
Show what's in an app file (name, id, format, tables, files)

Usage: uapp info [OPTIONS] <APP>

Arguments:
  <APP>  The .uapp file

Options:
      --json                  Print JSON
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file
  -h, --help                  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp encrypt

```text
Encrypt an app with a master password

Usage: uapp encrypt [OPTIONS] <FILE>

Arguments:
  <FILE>  The .uapp file

Options:
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file
  -h, --help                  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp decrypt

```text
Remove an app's encryption

Usage: uapp decrypt [OPTIONS] <FILE>

Arguments:
  <FILE>  The .uapp file

Options:
      --password <PW>         Master password (visible to other users in `ps`;
                              prefer --password-stdin, --password-file or
                              UAPP_PASSWORD)
      --password-stdin        Read the master password from the first line of
                              stdin
      --password-file <PATH>  Read the master password from the first line of a
                              file
  -h, --help                  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp passwd

```text
Change an app's master password.

The new password comes from UAPP_NEW_PASSWORD, else the second line of stdin
(with --password-stdin), else a prompt.

Usage: uapp passwd [OPTIONS] <FILE>

Arguments:
  <FILE>
          The .uapp file

Options:
      --password <PW>
          Master password (visible to other users in `ps`; prefer
          --password-stdin, --password-file or UAPP_PASSWORD)

      --password-stdin
          Read the master password from the first line of stdin

      --password-file <PATH>
          Read the master password from the first line of a file

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -v, --verbose...
          Log more (-v debug, -vv trace). UAPP_LOG overrides both flags

  -q, --quiet...
          Log less (-q warnings only, -qq errors only)
```

### uapp serve

```text
Serve an app as a public website, or the browser build on its own.

With FILE, anyone can read the site's pages; the token unlocks the editing
shell, the WebSocket and publishing (PUT /site.uapp).

Without FILE, --chrome is required: the browser build is served alone and opens
whatever its URL names (?open=<url>, ?app=<id>, or --open).

Usage: uapp serve [OPTIONS] [FILE]

Arguments:
  [FILE]
          The .uapp to serve as a website (none: serve only the browser build)

Options:
      --bind <ADDR>
          Interface to bind (0.0.0.0 accepts outside connections)
          
          [default: 127.0.0.1]

      --port <N>
          Port to listen on
          
          [default: 8080]

      --token <T>
          Secret that unlocks the shell, the WebSocket and publishing (default:
          a random one, printed at start)
          
          [env: UAPP_TOKEN]

      --coi
          Send COOP/COEP so pages are cross-origin isolated (needed by the
          editing chrome's run_js bridge; breaks cross-origin iframe embeds such
          as YouTube)

      --no-coi
          Without FILE: don't send COOP/COEP (sent by default there, since only
          the shell is served)

      --no-archive
          Don't expose /site.uapp

      --publish-data
          Include data/ (content sources) in /site.uapp so visitors can re-run
          the site's build. Everything in the archive is public

      --asset-max-age <SECS>
          Let browsers cache non-HTML site files this long (only if your build
          fingerprints asset names)
          
          [default: 0]

      --chrome <DIR>
          Serve the browser build from this directory (dist-web), which gives
          pages an "Edit this site" button that edits a local copy

      --open <URL>
          Without FILE: what a bare visit opens, e.g. /uapp/apps/launcher.uapp
          on a library

      --password <PW>
          Master password (visible to other users in `ps`; prefer
          --password-stdin, --password-file or UAPP_PASSWORD)

      --password-stdin
          Read the master password from the first line of stdin

      --password-file <PATH>
          Read the master password from the first line of a file

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -v, --verbose...
          Log more (-v debug, -vv trace). UAPP_LOG overrides both flags

  -q, --quiet...
          Log less (-q warnings only, -qq errors only)
```

### uapp library

```text
Serve a folder of .uapp files over HTTP.

GET / lists them as JSON and GET /<name>.uapp fetches one (ETag, If-None-Match).
PUT /<name>.uapp publishes one (needs the token; If-Match gives 409 on a
conflict) and is stored without chat, history, API keys, MCP credentials or
approvals. DELETE /<name>.uapp removes one (needs the token).

Usage: uapp library [OPTIONS] <DIR>

Arguments:
  <DIR>
          Folder of .uapp files (created if missing)

Options:
      --bind <ADDR>
          Interface to bind (0.0.0.0 accepts outside connections)
          
          [default: 127.0.0.1]

      --port <N>
          Port to listen on
          
          [default: 8090]

      --token <T>
          Secret that unlocks PUT and DELETE (default: a random one, printed at
          start)
          
          [env: UAPP_TOKEN]

      --max-mb <N>
          Largest upload accepted, in MB
          
          [default: 256]

  -h, --help
          Print help (see a summary with '-h')

Global options:
  -v, --verbose...
          Log more (-v debug, -vv trace). UAPP_LOG overrides both flags

  -q, --quiet...
          Log less (-q warnings only, -qq errors only)
```

### uapp install

```text
Register .uapp files with your desktop and file manager (Linux)

Usage: uapp install [OPTIONS]

Options:
  -h, --help  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp uninstall

```text
Remove what `uapp install` set up

Usage: uapp uninstall [OPTIONS]

Options:
  -h, --help  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)
```

### uapp completions

```text
Print a shell completion script

Usage: uapp completions [OPTIONS] <SHELL>

Arguments:
  <SHELL>  [possible values: bash, elvish, fish, powershell, zsh]

Options:
  -h, --help  Print help

Global options:
  -v, --verbose...  Log more (-v debug, -vv trace). UAPP_LOG overrides both
                    flags
  -q, --quiet...    Log less (-q warnings only, -qq errors only)

Example (bash): uapp completions bash >
~/.local/share/bash-completion/completions/uapp
```
