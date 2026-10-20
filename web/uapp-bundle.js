/* uapp browser build — the files of the bundle, listed once.
 *
 * scripts/build-web.sh copies each entry from its source into dist-web/ and
 * checks that every entry ended up there. sw.js loads this file
 * (importScripts) and treats every path in it as the shell's own, never
 * looked up in an archive. A new file in the bundle goes here and nowhere
 * else.
 *
 *   "<path in dist-web>": "<source in the repo>"  a file, or a directory when
 *                                                  both end in "/"
 *   "<path in dist-web>": null                     made by the build itself
 *
 * One entry per line, in exactly that form: build-web.sh reads the lines
 * with sed, not a JavaScript parser.
 */
self.UAPP_BUNDLE = {
  "index.html": "web/index.html",
  "boot.js": "web/boot.js",
  "worker.js": "web/worker.js",
  "sw.js": "web/sw.js",
  "uapp_glue.js": "web/uapp_glue.js",
  "site-chrome.js": "web/site-chrome.js",
  "sync-pill.js": "web/sync-pill.js",
  "uapp-bundle.js": "web/uapp-bundle.js",
  "uapp.js": "src/shell/uapp.js",
  "icons.js": "src/shell/icons.js",
  "scratch.html": "src/shell/scratch.html",
  "shell.css": "src/shell/shell.css",
  "shell/": "src/shell/js/",
  "icon-256.png": "assets/uapp-256.png",
  "uapp_wasm.js": null,
  "uapp_wasm_bg.wasm": null,
  "launcher.uapp": null,
  "uapp-build.json": null,
};
