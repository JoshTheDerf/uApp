// File browser back end: MIME types, HTTP Range requests, files.rename
// (files and folders), the app//data/ split and its migration, POST /upload,
// files.present (broadcast to every WS client) and the present_file tool.
import { spawn, execFileSync } from "node:child_process";
import { rmSync, mkdirSync, writeFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";
import { randomBytes } from "node:crypto";

const BIN = process.env.UAPP_BIN || "/home/sysadmin/Experiments/uapp/target/debug/uapp";
const DIR = process.env.TESTDIR || "/tmp/uapp-test-filebrowser";
const FILE = `${DIR}/fb.uapp`;

let passed = 0, failed = 0;
function ok(cond, name, extra = "") {
  if (cond) { passed++; console.log(`  ✓ ${name}`); }
  else { failed++; console.log(`  ✗ FAIL: ${name} ${extra}`); }
}

class Instance {
  constructor(device, file = FILE) {
    this.device = device;
    this.file = file;
  }
  async start() {
    this.proc = spawn(BIN, ["open", this.file, "--headless"], {
      env: { ...process.env, UAPP_DEVICE: this.device, UAPP_LINGER_SECS: "600" },
      stdio: ["ignore", "pipe", "pipe"],
    });
    this.stderr = "";
    this.proc.stderr.on("data", (d) => { this.stderr += d; });
    const line = await new Promise((resolve, reject) => {
      let buf = "";
      const t = setTimeout(() => reject(new Error(`${this.device}: no startup line. stderr: ${this.stderr}`)), 15000);
      this.proc.stdout.on("data", (d) => {
        buf += d;
        const nl = buf.indexOf("\n");
        if (nl >= 0) { clearTimeout(t); resolve(buf.slice(0, nl)); }
      });
      this.proc.on("exit", (c) => { clearTimeout(t); reject(new Error(`${this.device} exited ${c}: ${this.stderr}`)); });
    });
    const info = JSON.parse(line);
    this.port = info.port;
    this.token = new URL(info.url).searchParams.get("t");
    await this.connect();
    return info;
  }
  async connect() {
    this.ws = new WebSocket(`ws://127.0.0.1:${this.port}/ws?t=${this.token}`);
    this.nextId = 1;
    this.pending = new Map();
    this.events = [];
    await new Promise((res, rej) => {
      this.ws.onopen = res;
      this.ws.onerror = () => rej(new Error("ws error"));
    });
    this.ws.onmessage = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.method === "event") { this.events.push(m.params); return; }
      const p = this.pending.get(m.id);
      if (p) { this.pending.delete(m.id); m.error ? p.reject(new Error(m.error.message)) : p.resolve(m.result); }
    };
  }
  rpc(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.pending.set(id, { resolve, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => { if (this.pending.has(id)) { this.pending.delete(id); reject(new Error(`rpc timeout: ${method}`)); } }, 20000);
    });
  }
  /// Raw HTTP with the cookie the shell page would carry.
  fetch(path, headers = {}) {
    return fetch(`http://127.0.0.1:${this.port}${path}`, {
      headers: { Cookie: `uapp_t_${this.port}=${this.token}`, ...headers },
    });
  }
  async stop() { try { this.ws?.close(); } catch {} this.proc.kill("SIGINT"); await sleep(300); this.proc.kill("SIGKILL"); }
}

async function waitFor(fn, ms = 8000, step = 200) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try { const v = await fn(); if (v) return v; } catch {}
    await sleep(step);
  }
  return null;
}

rmSync(DIR, { recursive: true, force: true });
mkdirSync(DIR, { recursive: true });
// Lets [10] fabricate the native-window drops that only a desktop session
// would otherwise produce.
process.env.UAPP_TEST_DROPS = "1";

const A = new Instance("devfb1");
await A.start();

console.log("\n[1] content types for viewer media");
{
  await A.rpc("files.write", { name: "notes/report.md", text: "# Title\n\nhello\n" });
  // Not a real container — only the extension drives the served MIME type.
  const clip = randomBytes(4096);
  await A.rpc("files.write", { name: "media/clip.mp4", b64: clip.toString("base64") });
  const md = await A.fetch("/app/notes/report.md");
  ok(md.headers.get("content-type") === "text/markdown; charset=utf-8",
    "markdown served as text/markdown", md.headers.get("content-type"));
  const mp4 = await A.fetch("/app/media/clip.mp4");
  ok(mp4.headers.get("content-type") === "video/mp4", "mp4 served as video/mp4", mp4.headers.get("content-type"));
  ok(mp4.headers.get("accept-ranges") === "bytes", "200 responses advertise byte ranges");
  ok((await mp4.arrayBuffer()).byteLength === 4096, "full body served without a Range header");
  globalThis.clip = clip;
}

console.log("\n[2] Range requests");
{
  const clip = globalThis.clip;
  const r = await A.fetch("/app/media/clip.mp4", { Range: "bytes=100-199" });
  ok(r.status === 206, "range request answers 206", r.status);
  ok(r.headers.get("content-range") === "bytes 100-199/4096", "Content-Range names the slice", r.headers.get("content-range"));
  ok(r.headers.get("accept-ranges") === "bytes", "206 advertises byte ranges");
  const body = Buffer.from(await r.arrayBuffer());
  ok(body.length === 100 && body.equals(clip.subarray(100, 200)), "the returned bytes are the requested slice");

  const open = await A.fetch("/app/media/clip.mp4", { Range: "bytes=4000-" });
  const openBody = Buffer.from(await open.arrayBuffer());
  ok(open.status === 206 && openBody.length === 96 &&
     open.headers.get("content-range") === "bytes 4000-4095/4096", "open-ended range runs to the end");

  const suffix = await A.fetch("/app/media/clip.mp4", { Range: "bytes=-10" });
  ok(suffix.status === 206 && suffix.headers.get("content-range") === "bytes 4086-4095/4096",
    "suffix range returns the last bytes", suffix.headers.get("content-range"));

  const bad = await A.fetch("/app/media/clip.mp4", { Range: "bytes=9000-9100" });
  ok(bad.status === 416, "out-of-bounds range is 416", bad.status);
  ok(bad.headers.get("content-range") === "bytes */4096", "416 reports the full size", bad.headers.get("content-range"));

  const junk = await A.fetch("/app/media/clip.mp4", { Range: "rows=1-2" });
  ok(junk.status === 200 && (await junk.arrayBuffer()).byteLength === 4096,
    "malformed Range header is ignored (full 200)");
  const multi = await A.fetch("/app/media/clip.mp4", { Range: "bytes=0-9,20-29" });
  ok(multi.status === 200 && (await multi.arrayBuffer()).byteLength === 4096,
    "multi-range is ignored (full 200)");
}

console.log("\n[3] files.rename");
{
  // Names written without a root were folded into app/ (see [6]).
  const r = await A.rpc("files.rename", { from: "app/notes/report.md", to: "app/notes/q3-report.md" });
  ok(r.ok && r.to === "app/notes/q3-report.md", "rename reports the new name");
  const list = (await A.rpc("files.list")).rows.map((x) => x[0]);
  ok(list.includes("app/notes/q3-report.md") && !list.includes("app/notes/report.md"),
    "the file moved in the archive", JSON.stringify(list));
  const back = await A.rpc("files.read", { name: "notes/q3-report.md" });
  ok(back.text === "# Title\n\nhello\n", "content survives the rename");
  let err = "";
  await A.rpc("files.rename", { from: "notes/q3-report.md", to: "media/clip.mp4" }).catch((e) => { err = e.message; });
  ok(/already exists/.test(err), "renaming onto an existing file is refused", err);
  err = "";
  await A.rpc("files.rename", { from: "notes/q3-report.md", to: "../escape.md" }).catch((e) => { err = e.message; });
  ok(/bad target name/.test(err), "'..' in the target is refused", err);
  err = "";
  await A.rpc("files.rename", { from: "nope.txt", to: "yes.txt" }).catch((e) => { err = e.message; });
  ok(/no such file/.test(err), "renaming a missing file errors", err);
}

console.log("\n[4] files.present reaches every client on this device");
{
  const second = new Instance("devfb1"); // same process, second socket
  second.port = A.port;
  second.token = A.token;
  await second.connect();
  A.events.length = 0;
  const r = await A.rpc("files.present", { name: "notes/q3-report.md", mode: "pretty" });
  ok(r.ok === true, "files.present accepted");
  const evA = await waitFor(() => A.events.find((e) => e.type === "present"));
  const evB = await waitFor(() => second.events.find((e) => e.type === "present"));
  ok(!!evA && !!evB, "both WS clients got the present event");
  ok(evA && evA.name === "app/notes/q3-report.md" && evA.mode === "pretty",
    "event carries the file and mode", JSON.stringify(evA));
  ok(evA && evA.by && evA.by !== "assistant",
    "a user-driven present is attributed to the engine user", evA && evA.by);
  let err = "";
  await A.rpc("files.present", { name: "no/such.txt" }).catch((e) => { err = e.message; });
  ok(/no such file/.test(err), "presenting a missing file errors", err);
  err = "";
  await A.rpc("files.present", { name: "media/clip.mp4", mode: "sideways" }).catch((e) => { err = e.message; });
  ok(/mode must be/.test(err), "unknown mode is refused", err);
  // The wire can't forge assistant attribution (the marker is stripped).
  second.events.length = 0;
  await A.rpc("files.present", { name: "media/clip.mp4", _assistant: true });
  const ev2 = await waitFor(() => second.events.find((e) => e.type === "present"));
  ok(ev2 && ev2.by !== "assistant", "clients cannot claim to be the assistant", ev2 && ev2.by);
  try { second.ws.close(); } catch {}
}

console.log("\n[5] present_file tool");
{
  const tools = await A.rpc("tools.list");
  const t = tools.find((x) => x.name === "present_file");
  ok(!!t, "present_file is in tools.list");
  ok(t && t.gated === false, "present_file is ungated (display only)");
  A.events.length = 0;
  const r = await A.rpc("tools.call", { name: "present_file", input: { name: "media/clip.mp4" } });
  ok(r && r.ok === true, "the tool runs", JSON.stringify(r));
  const ev = await waitFor(() => A.events.find((e) => e.type === "present"));
  ok(ev && ev.by === "assistant" && ev.mode === "auto",
    "tool presents are attributed to the assistant, mode defaults to auto", JSON.stringify(ev));
}

console.log("\n[6] the archive's two roots: app/ (code) and data/ (user content)");
{
  const list = async () => (await A.rpc("files.list")).rows;
  const rows = await list();
  ok(rows.every(([n]) => n.startsWith("app/") || n.startsWith("data/")),
    "every name lives under app/ or data/", JSON.stringify(rows.map((r) => r[0])));
  ok(rows.find(([n]) => n === "app/index.html"), "the bootstrap page is app/index.html");
  // A path with no root folds into app/; the role column follows the location.
  const w = await A.rpc("files.write", { name: "css/site.css", text: "body{}" });
  ok(w.name === "app/css/site.css", "an unprefixed write lands in app/", w.name);
  await A.rpc("files.write", { name: "data/sales.csv", text: "a,b\n1,2\n" });
  const roles = Object.fromEntries((await list()).map((r) => [r[0], r[3]]));
  ok(roles["app/css/site.css"] === "app" && roles["data/sales.csv"] === "data",
    "role is the directory", JSON.stringify(roles));
  // Legacy references still resolve to their canonical home.
  const legacy = await A.rpc("files.read", { name: "/css/site.css" });
  ok(legacy.name === "app/css/site.css" && legacy.text === "body{}",
    "a root-absolute reference resolves into app/", legacy.name);
  const served = await A.fetch("/css/site.css");
  ok(served.status === 200 && (await served.text()) === "body{}",
    "and it is still served at the root path", served.status);
}

console.log("\n[7] folder rename moves everything under it");
{
  await A.rpc("files.write", { name: "app/reports/q1.md", text: "one" });
  await A.rpc("files.write", { name: "app/reports/deep/q2.md", text: "two" });
  const r = await A.rpc("files.rename", { from: "app/reports/", to: "app/archive/" });
  ok(r.ok && r.moved === 2, `both files moved (${r.moved})`);
  const names = (await A.rpc("files.list")).rows.map((x) => x[0]);
  ok(names.includes("app/archive/q1.md") && names.includes("app/archive/deep/q2.md"),
    "nested paths keep their shape", JSON.stringify(names));
  ok(!names.some((n) => n.startsWith("app/reports/")), "the old folder is gone");
  const back = await A.rpc("files.read", { name: "app/archive/deep/q2.md" });
  ok(back.text === "two", "content survives a folder rename");

  // Moving a folder between the roots is how its files change role.
  const mv = await A.rpc("files.rename", { from: "app/archive/", to: "data/archive/" });
  ok(mv.ok && mv.moved === 2, "folder moved across roots");
  const after = (await A.rpc("files.list")).rows;
  ok(after.filter(([n]) => n.startsWith("data/archive/")).every((r) => r[3] === "data"),
    "the moved files are data now", JSON.stringify(after));

  let err = "";
  await A.rpc("files.rename", { from: "app/", to: "code/" }).catch((e) => { err = e.message; });
  ok(/can't be renamed/.test(err), "the app/ root itself is protected", err);
  err = "";
  await A.rpc("files.rename", { from: "data/archive/", to: "data/archive/inner/" })
    .catch((e) => { err = e.message; });
  ok(/into itself/.test(err), "a folder can't be moved inside itself", err);
  err = "";
  await A.rpc("files.rename", { from: "app/nothing/", to: "app/somewhere/" })
    .catch((e) => { err = e.message; });
  ok(/no such folder/.test(err), "renaming a missing folder errors", err);
  err = "";
  await A.rpc("files.write", { name: "data/clash/q1.md", text: "x" });
  await A.rpc("files.rename", { from: "data/archive/", to: "data/clash/" })
    .catch((e) => { err = e.message; });
  ok(/already exists/.test(err), "a colliding destination file is refused", err);
}

console.log("\n[8] POST /upload stores raw bytes (the browser's upload path)");
{
  const body = randomBytes(2048);
  const r = await fetch(`http://127.0.0.1:${A.port}/upload?name=${encodeURIComponent("data/blob.bin")}`, {
    method: "POST",
    headers: { Cookie: `uapp_t_${A.port}=${A.token}`, "content-type": "application/octet-stream" },
    body,
  });
  ok(r.status === 200, "upload accepted", r.status);
  const out = await r.json();
  ok(out.name === "data/blob.bin" && out.bytes === 2048, "reports what it stored", JSON.stringify(out));
  const back = await A.rpc("files.read", { name: "data/blob.bin" });
  ok(Buffer.from(back.b64, "base64").equals(body), "the bytes round-trip exactly");
  const noauth = await fetch(`http://127.0.0.1:${A.port}/upload?name=data/x.bin`,
    { method: "POST", body: "x" });
  ok(noauth.status === 403, "upload requires the session token", noauth.status);
  const bad = await fetch(`http://127.0.0.1:${A.port}/upload?name=${encodeURIComponent("../escape.bin")}`, {
    method: "POST",
    headers: { Cookie: `uapp_t_${A.port}=${A.token}` },
    body: "x",
  });
  ok(bad.status === 400, "'..' in the name is refused", bad.status);
}

console.log("\n[9] an archive from before the split migrates when it opens");
{
  const L = new Instance("devfb2", `${DIR}/legacy.uapp`);
  await L.start();
  await L.rpc("files.write", { name: "app/js/app.js", text: "// first" });
  await L.rpc("files.write", { name: "app/js/app.js", text: "// code" }); // makes a history row
  await L.rpc("files.write", { name: "data/upload.csv", text: "a,b\n" });
  await L.rpc("files.write", { name: "app/seed.csv", text: "x\n" });
  await L.rpc("app.compact"); // VACUUM: the file is already the state
  await L.stop();
  // Rewrite the checkpoint into the OLD layout by hand: code at the root,
  // user data under uploads/, plus a file_roles override marking a root file
  // as data (the thing the directories replace).
  execFileSync("sqlite3", [`${DIR}/legacy.uapp`,
    "UPDATE sqlar SET name='index.html' WHERE name='app/index.html';" +
    "UPDATE sqlar SET name='js/app.js' WHERE name='app/js/app.js';" +
    "UPDATE sqlar SET name='uploads/upload.csv' WHERE name='data/upload.csv';" +
    "UPDATE sqlar SET name='seed.csv' WHERE name='app/seed.csv';" +
    "UPDATE uapp_file_history SET name='js/app.js' WHERE name='app/js/app.js';" +
    `INSERT OR REPLACE INTO uapp_config(key,value) VALUES('file_roles','{"seed.csv":"data"}');`]);

  const L2 = new Instance("devfb2", `${DIR}/legacy.uapp`);
  await L2.start();
  const names = (await L2.rpc("files.list")).rows.map((r) => r[0]);
  ok(names.includes("app/index.html") && names.includes("app/js/app.js"),
    "root code files moved under app/", JSON.stringify(names));
  ok(names.includes("data/upload.csv"), "uploads/ became data/", JSON.stringify(names));
  ok(names.includes("data/seed.csv") && !names.includes("app/seed.csv"),
    "an old file_roles override decided where its file landed", JSON.stringify(names));
  ok((await L2.rpc("config.get", { key: "file_roles" })) === null,
    "the file_roles map is dropped — the directory is the role now");
  const hist = await L2.rpc("files.history", { name: "app/js/app.js" });
  ok(hist.rows.length >= 1, "file history followed the rename", JSON.stringify(hist.rows));
  const served = await L2.fetch("/app/js/app.js");
  ok(served.status === 200 && (await served.text()) === "// code",
    "old references still serve", served.status);
  await L2.stop();
}

console.log("\n[10] native-window file drops are relayed, then ingested by path");
{
  writeFileSync(`${DIR}/native-a.txt`, "alpha");
  writeFileSync(`${DIR}/native-b.txt`, "beta");
  mkdirSync(`${DIR}/a-folder`, { recursive: true });
  A.events.length = 0;
  await A.rpc("drop.simulate", { phase: "hover", x: 120, y: 240 });
  const hover = await waitFor(() => A.events.find((e) => e.type === "drop_hover"));
  ok(hover && hover.x === 120 && hover.y === 240, "hover is relayed with the cursor position",
    JSON.stringify(hover));
  await A.rpc("drop.simulate", { phase: "leave" });
  ok(!!(await waitFor(() => A.events.find((e) => e.type === "drop_leave"))), "leave is relayed");

  A.events.length = 0;
  await A.rpc("drop.simulate", {
    phase: "drop",
    paths: [`${DIR}/native-a.txt`, `${DIR}/native-b.txt`, `${DIR}/a-folder`],
    x: 300, y: 300,
  });
  const drop = await waitFor(() => A.events.find((e) => e.type === "drop_files"));
  ok(!!drop && !!drop.id, "the drop reaches the page with an id", JSON.stringify(drop));
  ok(drop && drop.names.join(",") === "native-a.txt,native-b.txt,a-folder",
    "the page is told the names only — never the OS paths", JSON.stringify(drop?.names));
  const r = await A.rpc("drop.ingest", { id: drop.id, dir: "data/inbox/" });
  ok(r.files.length === 2, `both files stored (${r.files.length})`);
  ok(r.files.every((f) => f.name.startsWith("data/inbox/")), "stored where the page asked",
    JSON.stringify(r.files));
  ok(r.skipped.length === 1 && /folders/.test(r.skipped[0]), "the folder is reported, not walked",
    JSON.stringify(r.skipped));
  const back = await A.rpc("files.read", { name: "data/inbox/native-a.txt" });
  ok(back.text === "alpha", "content came from the real file on disk");

  let err = "";
  await A.rpc("drop.ingest", { id: drop.id, dir: "data/" }).catch((e) => { err = e.message; });
  ok(/expired/.test(err), "an id is good for exactly one ingest", err);
  err = "";
  await A.rpc("drop.ingest", { id: "made-up", dir: "data/" }).catch((e) => { err = e.message; });
  ok(/expired/.test(err), "a made-up id ingests nothing", err);
}

await A.stop();
console.log(`\n==== ${passed} passed, ${failed} failed ====`);
process.exit(failed ? 1 : 0);
