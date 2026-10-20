# Security

## Reporting a vulnerability

Please report it privately through GitHub: on the [uApp repository](https://github.com/JoshTheDerf/uApp), go to **Security > Report a vulnerability**. That opens a private security advisory only you and the maintainer can see. Please don't open a public issue for it.

It helps to include what's affected (the desktop app, Android, `uapp serve`, `uapp library` or the browser build), the version (`uapp --version`), and the steps or a `.uapp` file that shows the problem. I'll reply in the advisory, and once a fix is out the advisory gets published with credit to you (unless you'd rather not be named).

## Supported versions

Only the latest release gets security fixes. If you're on an older one, update first and check that the problem is still there.

## The trust model, in short

- **Apps are kept apart from the editor.** On the desktop and Android an app's JavaScript runs on its own origin (`localhost`, next to the editor's `127.0.0.1`) and its connection only gets the calls an app needs: its own data and files, the tools (gated ones ask you, and only the editor can answer) and its own `app.*` settings. It can't change your AI or MCP setup, read your chats or approve its own prompts. Apps on the same port share that origin with each other. See [docs/design/app-isolation.md](docs/design/app-isolation.md).
- **The browser build has one origin for everything**, so an app opened there can do anything you can do in the editor, including reaching every other app stored in that browser. Settings says so. See [docs/design/web-build-isolation.md](docs/design/web-build-isolation.md).
- **Your secrets stay on your device.** On desktop and Android, the AI API key, MCP credentials and "always allow" answers are kept outside the `.uapp` and never travel with it. The browser build keeps them in its local copy and strips them from downloads and publishes.
- **App SQL is sandboxed.** It can't touch uApp's own tables, attach other databases or change settings like the encryption key. See [docs/APP-API.md](docs/APP-API.md#what-sql-can-and-cant-do).
- **The local server only answers you.** It listens on loopback only (127.0.0.1 and ::1), needs a random per-session token, and refuses requests from other origins (other localhost ports included).
- **`uapp serve` and `uapp library` are public by design.** Anyone can read a served site's pages, its `/site.uapp` and a library's files. The token only guards editing and publishing, so keep it secret and put the server behind HTTPS.

Reports about any of the above are welcome, especially ways around the parts that are supposed to hold today.
