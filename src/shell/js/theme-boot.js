// Apply a pinned Light/Dark theme before the first paint. A classic script,
// loaded from <head>: the shell's CSP allows no inline script, and waiting for
// the modules (theme.js) would flash the other theme first.
try {
  var t = localStorage.getItem("uapp.theme");
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
} catch (e) {}
