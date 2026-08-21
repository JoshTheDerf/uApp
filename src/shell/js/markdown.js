/* uapp shell — markdown renderer + lightweight syntax highlighting, shared by
 * the chat bubbles and the file viewer. Escape-FIRST throughout (see ui.js). */

import { esc, appUrl, installStyle } from "./ui.js";

// Language families for the tokenizer. Each is a few regex fragments; enough
// to make code readable without vendoring a highlighter.
const HL_FAMILIES = {
  js: { line: "//", block: true, tick: true, kw: "await|async|break|case|catch|class|const|continue|debugger|default|delete|do|else|export|extends|finally|for|from|function|get|if|import|in|instanceof|let|new|null|of|return|set|static|super|switch|this|throw|true|false|try|typeof|undefined|var|void|while|yield" },
  rs: { line: "//", block: true, kw: "as|async|await|break|const|continue|crate|dyn|else|enum|extern|false|fn|for|if|impl|in|let|loop|match|mod|move|mut|pub|ref|return|self|Self|static|struct|super|trait|true|type|unsafe|use|where|while" },
  py: { line: "#", triple: true, kw: "and|as|assert|async|await|break|class|continue|def|del|elif|else|except|False|finally|for|from|global|if|import|in|is|lambda|None|nonlocal|not|or|pass|raise|return|True|try|while|with|yield" },
  sh: { line: "#", kw: "if|then|elif|else|fi|for|while|do|done|case|esac|function|return|export|local|in|set" },
  sql: { line: "--", block: true, ci: true, kw: "select|from|where|insert|into|values|update|set|delete|create|table|view|index|drop|alter|add|join|left|right|inner|outer|on|group|by|order|having|limit|offset|as|and|or|not|null|is|in|like|between|distinct|union|all|primary|key|foreign|references|default|integer|text|real|blob|begin|commit|rollback|pragma|with|case|when|then|else|end|exists|if|replace|autoincrement|unique" },
  css: { block: true, kw: "important|inherit|initial|unset|auto|none" },
  conf: { line: "#", kw: "true|false|null|yes|no" },
  html: { markup: true },
  json: { kw: "true|false|null" },
};
export const HL_BY_EXT = {
  js: "js", mjs: "js", cjs: "js", jsx: "js", ts: "js", tsx: "js",
  rs: "rs", go: "js", java: "js", c: "js", h: "js", cpp: "js", cs: "js", php: "js",
  py: "py", rb: "py", sh: "sh", bash: "sh", zsh: "sh",
  sql: "sql", css: "css", scss: "css",
  yaml: "conf", yml: "conf", toml: "conf", ini: "conf", conf: "conf", env: "conf",
  html: "html", htm: "html", xml: "html", svg: "html", vue: "html",
  json: "json",
};
/// Escape `code` and wrap recognised tokens in <span class="tok-…">.
export function hlCode(code, lang) {
  const f = HL_FAMILIES[HL_BY_EXT[String(lang || "").toLowerCase()] || ""];
  if (!f) return esc(code);
  const specs = [];
  if (f.markup) {
    specs.push(["cmt", "<!--[\\s\\S]*?-->"]);
    specs.push(["str", "\"[^\"\\n]*\"|'[^'\\n]*'"]);
    specs.push(["kw", "</?[A-Za-z][\\w:.-]*|/?>"]);
  } else {
    if (f.block) specs.push(["cmt", "/\\*[\\s\\S]*?\\*/"]);
    if (f.line) specs.push(["cmt", f.line.replace(/([/*#-])/g, "\\$1") + "[^\\n]*"]);
    const str = (f.triple ? "\"\"\"[\\s\\S]*?\"\"\"|'''[\\s\\S]*?'''|" : "") +
      "\"(?:[^\"\\\\\\n]|\\\\.)*\"|'(?:[^'\\\\\\n]|\\\\.)*'" +
      (f.tick ? "|`(?:[^`\\\\]|\\\\.)*`" : "");
    specs.push(["str", str]);
    if (f.kw) specs.push(["kw", "\\b(?:" + f.kw + ")\\b"]);
    specs.push(["num", "\\b\\d[\\d_]*(?:\\.\\d+)?(?:[eE][+-]?\\d+)?\\b"]);
  }
  const re = new RegExp(specs.map(([, src]) => "(" + src + ")").join("|"), f.ci ? "gi" : "g");
  let out = "", last = 0, m;
  while ((m = re.exec(code))) {
    if (m[0] === "") { re.lastIndex++; continue; }
    out += esc(code.slice(last, m.index));
    const gi = specs.findIndex((_, k) => m[k + 1] !== undefined);
    out += `<span class="tok-${specs[gi][0]}">${esc(m[0])}</span>`;
    last = m.index + m[0].length;
  }
  return out + esc(code.slice(last));
}

/// Only http(s) links survive — no javascript:/data: anywhere in the shell.
function mdLinkUrl(u) {
  return /^https?:\/\/[^\s"'<>]+$/i.test(u) ? u : null;
}
/// Images: same-origin archive files only. Relative names resolve against the
/// folder of the file being rendered (opts.base), never above the archive.
function mdImgUrl(u, base) {
  if (u.includes("..") || /^[a-z][a-z0-9+.-]*:/i.test(u)) return null;
  if (u.startsWith("/app/")) return u;
  if (u.startsWith("/")) return null;
  return appUrl((base || "") + u);
}
function mdInline(s, opts) {
  let h = esc(s);
  // Code spans are parked first so their contents can't be re-parsed.
  const spans = [];
  h = h.replace(/`([^`]+)`/g, (_, c) => `\u0000${spans.push(`<code>${c}</code>`) - 1}\u0000`);
  h = h.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (_, alt, url) => {
    const u = mdImgUrl(url, opts.base);
    return u ? `<img src="${u}" alt="${alt}" loading="lazy">` : alt;
  });
  h = h.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, txt, url) => {
    const u = mdLinkUrl(url);
    return u ? `<a href="${u}" target="_blank" rel="noopener noreferrer">${txt}</a>` : txt;
  });
  h = h.replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/(^|[\s(])\*([^*\n]+)\*/g, "$1<i>$2</i>")
    .replace(/(^|[\s(])_([^_\n]+)_(?=[\s.,;:!?)]|$)/g, "$1<i>$2</i>")
    .replace(/~~([^~]+)~~/g, "<s>$1</s>");
  return h.replace(/\u0000(\d+)\u0000/g, (_, i) => spans[+i]);
}
const MD_BLOCK = /^\s{0,3}(?:#{1,6}\s|>|(?:[-*+]|\d+[.)])\s|```|~~~)/;
/// Small block-level markdown renderer: headings, lists, quotes, fenced code,
/// tables, hr, links, images.
export function mdRender(src, opts = {}) {
  const lines = String(src == null ? "" : src).replace(/\r\n?/g, "\n").split("\n");
  const out = [];
  const stack = []; // open lists: {tag, indent}
  const closeLists = () => { while (stack.length) out.push(`</${stack.pop().tag}>`); };
  let i = 0;
  while (i < lines.length) {
    const ln = lines[i];
    const fence = ln.match(/^\s*(```|~~~)([\w+#-]*)\s*$/);
    if (fence) {
      closeLists();
      const close = new RegExp("^\\s*" + fence[1] + "+\\s*$");
      const buf = [];
      i++;
      while (i < lines.length && !close.test(lines[i])) buf.push(lines[i++]);
      i++; // closing fence (or EOF)
      out.push(`<pre class="mdcode"><code>${hlCode(buf.join("\n"), fence[2])}</code></pre>`);
      continue;
    }
    if (!ln.trim()) { closeLists(); i++; continue; }
    if (/^\s{0,3}([-*_])(\s*\1){2,}\s*$/.test(ln)) { closeLists(); out.push("<hr>"); i++; continue; }
    const head = ln.match(/^\s{0,3}(#{1,6})\s+(.*)$/);
    if (head) {
      closeLists();
      const lvl = head[1].length;
      out.push(`<h${lvl}>${mdInline(head[2].replace(/\s+#+\s*$/, ""), opts)}</h${lvl}>`);
      i++;
      continue;
    }
    if (/^\s{0,3}>/.test(ln)) {
      closeLists();
      const buf = [];
      while (i < lines.length && /^\s{0,3}>/.test(lines[i])) buf.push(lines[i++].replace(/^\s{0,3}>\s?/, ""));
      out.push(`<blockquote>${mdRender(buf.join("\n"), opts)}</blockquote>`);
      continue;
    }
    // table: a header row followed by a |---|---| separator
    if (ln.includes("|") && /^[\s|:-]*-[\s|:-]*$/.test(lines[i + 1] || "") && (lines[i + 1] || "").includes("|")) {
      closeLists();
      const cells = (l) => l.replace(/^\s*\|/, "").replace(/\|\s*$/, "").split("|").map((c) => c.trim());
      const heads = cells(ln);
      i += 2;
      const body = [];
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) body.push(cells(lines[i++]));
      out.push(`<table class="mdtable"><thead><tr>` +
        heads.map((c) => `<th>${mdInline(c, opts)}</th>`).join("") + `</tr></thead><tbody>` +
        body.map((r) => `<tr>${r.map((c) => `<td>${mdInline(c, opts)}</td>`).join("")}</tr>`).join("") +
        `</tbody></table>`);
      continue;
    }
    const item = ln.match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
    if (item) {
      const indent = item[1].replace(/\t/g, "  ").length;
      const tag = /^\d/.test(item[2]) ? "ol" : "ul";
      while (stack.length && indent < stack[stack.length - 1].indent) out.push(`</${stack.pop().tag}>`);
      const top = stack[stack.length - 1];
      if (!top || indent > top.indent) { out.push(`<${tag}>`); stack.push({ tag, indent }); }
      else if (top.tag !== tag) { out.push(`</${stack.pop().tag}><${tag}>`); stack.push({ tag, indent }); }
      out.push(`<li>${mdInline(item[3], opts)}</li>`);
      i++;
      continue;
    }
    const buf = [ln];
    i++;
    while (i < lines.length && lines[i].trim() && !MD_BLOCK.test(lines[i])) buf.push(lines[i++]);
    closeLists();
    out.push(`<p>${mdInline(buf.join("\n"), opts)}</p>`);
  }
  closeLists();
  return out.join("");
}

installStyle("markdown", /* css */ `
/* markdown body used by the viewer's pretty mode */
.mdbody p { margin: 0 0 10px; white-space: pre-wrap; }
.mdbody h1 { font-size: 22px; margin: 18px 0 8px; }
.mdbody h2 { font-size: 18px; margin: 16px 0 6px; }
.mdbody h3 { font-size: 15px; margin: 14px 0 6px; }
.mdbody h1:first-child, .mdbody h2:first-child { margin-top: 0; }
.mdbody ul, .mdbody ol { margin: 6px 0 10px; padding-left: 24px; }
.mdbody blockquote {
  margin: 8px 0; padding: 4px 12px; border-left: 3px solid var(--line-strong); color: var(--text-2);
}
.mdbody hr { border: none; border-top: 1px solid var(--line); margin: 16px 0; }
.mdbody img { max-width: 100%; border-radius: 6px; }
.mdbody a { color: var(--brand); }
.mdbody code {
  font-family: var(--mono); font-size: 12px;
  background: var(--code-bg); border-radius: 4px; padding: 1px 4px;
}
.mdbody pre.mdcode {
  background: var(--surface-2); border: 1px solid var(--line-soft); border-radius: 8px;
  padding: 10px 12px; overflow-x: auto; font: 12px/1.55 var(--mono);
}
.mdbody pre.mdcode code { background: none; padding: 0; }
.mdtable { border-collapse: collapse; font-size: 12px; margin: 6px 0; }
.mdtable th, .mdtable td { border: 1px solid var(--line); padding: 3px 8px; text-align: left; }
.mdtable th { background: var(--code-bg); }
pre.mdcode { white-space: pre; }

/* syntax highlighting tokens (viewer + fenced markdown code) */
.tok-cmt { color: var(--faint); font-style: italic; }
.tok-str { color: var(--ok-ink); }
.tok-kw  { color: var(--brand-ink); font-weight: 600; }
.tok-num { color: var(--warn-ink); }
`);
