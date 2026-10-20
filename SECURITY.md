# Security

## Reporting a vulnerability

Please report it privately through GitHub: on the [uApp repository](https://github.com/JoshTheDerf/uApp), go to **Security > Report a vulnerability**. That opens a private security advisory only you and the maintainer can see. Please don't open a public issue for it.

It helps to include what's affected (the desktop app, Android, `uapp serve`, `uapp library` or the browser build), the version (`uapp --version`), and the steps or a `.uapp` file that shows the problem. I'll reply in the advisory, and once a fix is out the advisory gets published with credit to you (unless you'd rather not be named).

## Supported versions

Only the latest release gets security fixes. If you're on an older one, update first and check that the problem is still there.

## The trust model, in short

- **A `.uapp` is a program.** An app's own JavaScript runs on the same origin as the uApp editor around it, so it can do anything you can do in the editor, including answering its own approval prompts. It can't read arbitrary files on your machine or run commands, but treat a `.uapp` from someone you don't trust the way you'd treat a program from them. [docs/design/app-isolation.md](docs/design/app-isolation.md) has the details and the plan to give apps their own origin.
- **The browser build has one origin for everything**, so an app opened there can also reach every other app stored in that browser. See [docs/design/web-build-isolation.md](docs/design/web-build-isolation.md).
- **Your secrets stay on your device.** On desktop and Android, the AI API key, MCP credentials and "always allow" answers are kept outside the `.uapp` and never travel with it. The browser build keeps them in its local copy and strips them from downloads and publishes.
- **App SQL is sandboxed.** It can't touch uApp's own tables, attach other databases or change settings like the encryption key. See [docs/APP-API.md](docs/APP-API.md#what-sql-can-and-cant-do).
- **The local server only answers you.** It listens on 127.0.0.1, needs a random per-session token, and refuses requests from other origins (other localhost ports included).
- **`uapp serve` and `uapp library` are public by design.** Anyone can read a served site's pages, its `/site.uapp` and a library's files. The token only guards editing and publishing, so keep it secret and put the server behind HTTPS.

Reports about any of the above are welcome, especially ways around the parts that are supposed to hold today.
