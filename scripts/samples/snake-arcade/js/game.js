/* Snake Arcade — uapp sample.
 * Levels come from a stored app file (levels.json); high scores live in SQLite
 * behind uapp actions, which double as AI-callable tools. */
"use strict";

// ---------- constants / state ----------
const COLS = 25, ROWS = 24, CELL = 20;
const canvas = document.getElementById("board");
const ctx = canvas.getContext("2d");

let levels = [];          // from levels.json
let levelIdx = 0;
let snake = [];           // [{x,y}] head first
let dir = { x: 1, y: 0 };
let nextDir = dir;
let food = null;
let score = 0;
let best = 0;
let walls = new Set();    // "x,y" keys
let timer = null;
let state = "idle";       // idle | running | paused | over

const $ = (id) => document.getElementById(id);

// ---------- sprites (stored app files under sprites/) ----------
// Loaded as Images at startup; if any fails we fall back to canvas primitives.
const sprites = { food: null, head: null, wall: null };
let spritesOk = false;

function loadSprites() {
  const load = (name) => new Promise((resolve) => {
    const img = new Image();
    img.onload = () => { sprites[name] = img; resolve(true); };
    img.onerror = () => resolve(false);
    img.src = `sprites/${name}.svg`;
  });
  return Promise.all(["food", "head", "wall"].map(load))
    .then((oks) => { spritesOk = oks.every(Boolean); });
}

// ---------- persistent game state (SQLite, throttled autosave) ----------
const SAVE_MIN_MS = 1500;   // at most one movement-save per 1.5s
let lastSaveAt = 0;
let saving = false;

function snapshot(alive) {
  return {
    snake, dir, food, score,
    levelIdx,
    alive,
    paused: state === "paused"
  };
}

async function saveState(alive, force) {
  const now = Date.now();
  if (!force && now - lastSaveAt < SAVE_MIN_MS) return;  // throttle movement saves
  if (saving) return;                                    // never overlap writes
  lastSaveAt = now;
  saving = true;
  try {
    await uapp.exec(
      "INSERT INTO game_state(id,state,updated) VALUES(1,?,?) " +
      "ON CONFLICT(id) DO UPDATE SET state=excluded.state, updated=excluded.updated",
      [JSON.stringify(snapshot(alive)), now]
    );
  } finally {
    saving = false;
  }
}

async function readSavedState() {
  const q = await uapp.query("SELECT state, updated FROM game_state WHERE id = 1");
  if (!q.rows.length) return null;
  try { return { ...JSON.parse(q.rows[0].state), updated: q.rows[0].updated }; }
  catch { return null; }
}

// ---------- actions (also exposed as AI tools) ----------
uapp.action("submit_score", {
  description: "Record a finished Snake Arcade game on the leaderboard. Inserts the player's name, final score and level reached, and returns the new row id plus the 1-based rank of that score among all scores.",
  params: {
    player: { type: "string" },
    score: { type: "number" },
    level: { type: "number" }
  }
}, async ({ player, score, level }) => {
  const ts = Date.now();
  const res = await uapp.exec(
    "INSERT INTO scores(player, score, level, ts) VALUES(?,?,?,?)",
    [String(player).slice(0, 24), Math.floor(score), Math.floor(level), ts]
  );
  const q = await uapp.query(
    "SELECT COUNT(*) AS better FROM scores WHERE score > ?", [Math.floor(score)]
  );
  return { id: res.insertId, rank: q.rows[0].better + 1 };
});

uapp.action("top_scores", {
  description: "Read the Snake Arcade leaderboard: the highest scores (default 10) with player name, score, level reached, and timestamp (ms since epoch), best first.",
  params: { limit: { type: "number", required: false } },
  readonly: true
}, async ({ limit }) => {
  const n = Math.max(1, Math.min(100, Math.floor(limit || 10)));
  const q = await uapp.query(
    "SELECT player, score, level, ts FROM scores ORDER BY score DESC, ts ASC LIMIT ?", [n]
  );
  return q.rows;
});

uapp.action("clear_scores", {
  description: "Delete ALL Snake Arcade leaderboard entries. Irreversible; use only when the user asks to reset the leaderboard.",
  params: {}
}, async () => {
  const res = await uapp.exec("DELETE FROM scores");
  return { deleted: res.changes };
});

uapp.action("current_game", {
  description: "Read the current Snake Arcade game as of its last autosave: snake position, direction, food, score, level, and whether the game is alive/paused, plus a one-line human summary. Autosaves happen on eating, level changes, pause, game over, and about every 1.5s while moving.",
  params: {},
  readonly: true
}, async () => {
  const saved = await readSavedState();
  if (!saved) return { summary: "No game has been played yet.", state: null };
  const lv = levels[saved.levelIdx];
  const summary = saved.alive
    ? `${saved.paused ? "Paused" : "Live"} game: score ${saved.score}, level ${saved.levelIdx + 1}${lv ? ` (${lv.name})` : ""}, snake length ${saved.snake.length}.`
    : `Last game ended with score ${saved.score} on level ${saved.levelIdx + 1}.`;
  return { summary, state: saved };
});

// ---------- leaderboard ----------
const esc = (s) => String(s).replace(/[&<>"']/g,
  (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

async function renderLeaderboard() {
  const rows = await uapp.call("top_scores", { limit: 10 });
  best = rows.length ? rows[0].score : 0;
  $("best").textContent = best;
  $("scores").innerHTML = rows.map((r) =>
    `<li><span class="p">${esc(r.player)}</span><span class="s">${r.score}</span><span class="l">L${r.level}</span></li>`
  ).join("") || "<li class='empty'>No scores yet</li>";
}

// ---------- level / board helpers ----------
const key = (x, y) => `${x},${y}`;

function loadLevel(i) {
  levelIdx = i;
  const lv = levels[i];
  walls = new Set(lv.walls.map(([x, y]) => key(x, y)));
  $("level").textContent = i + 1;
  $("level-name").textContent = lv.name;
  if (timer) clearInterval(timer);
  timer = setInterval(tick, lv.speedMs);
}

function placeFood() {
  const taken = new Set(snake.map((s) => key(s.x, s.y)));
  let x, y;
  do {
    x = Math.floor(Math.random() * COLS);
    y = Math.floor(Math.random() * ROWS);
  } while (taken.has(key(x, y)) || walls.has(key(x, y)));
  food = { x, y };
}

function startGame() {
  snake = [{ x: 12, y: 12 }, { x: 11, y: 12 }, { x: 10, y: 12 }];
  dir = nextDir = { x: 1, y: 0 };
  score = 0;
  $("score").textContent = "0";
  loadLevel(0);
  placeFood();
  state = "running";
  hideOverlay();
  saveState(true, true);
  draw();
}

// Restore an autosaved game exactly as it was.
function resumeGame(saved) {
  snake = saved.snake;
  dir = nextDir = saved.dir;
  food = saved.food;
  score = saved.score;
  $("score").textContent = score;
  loadLevel(saved.levelIdx);
  state = "running";
  hideOverlay();
  draw();
}

// ---------- game loop ----------
function tick() {
  if (state !== "running") return;
  dir = nextDir;
  const head = {
    x: (snake[0].x + dir.x + COLS) % COLS,   // wrap at edges
    y: (snake[0].y + dir.y + ROWS) % ROWS
  };
  const hitSelf = snake.some((s) => s.x === head.x && s.y === head.y);
  if (hitSelf || walls.has(key(head.x, head.y))) return gameOver();

  snake.unshift(head);
  if (head.x === food.x && head.y === food.y) {
    score += 10;
    $("score").textContent = score;
    placeFood();
    // level up?
    if (score >= levels[levelIdx].goal && levelIdx < levels.length - 1) {
      loadLevel(levelIdx + 1);
      placeFood();
    }
    saveState(true, true);      // event save: food eaten / level change
  } else {
    snake.pop();
    saveState(true, false);     // throttled movement save
  }
  draw();
}

function gameOver() {
  state = "over";
  clearInterval(timer);
  timer = null;
  saveState(false, true);       // mark saved state as dead
  $("btn-resume").classList.add("hidden");
  showOverlay("Game Over", `Score ${score} · reached level ${levelIdx + 1}`);
  const form = $("score-form");
  form.classList.remove("hidden");
  $("save-result").classList.add("hidden");
  const input = $("player-name");
  input.value = localStorage.getItem("snake.player") || "";
  input.focus();
  $("btn-start").textContent = "Play again";
}

function togglePause() {
  if (state === "running") {
    state = "paused";
    showOverlay("Paused", "Space or ⏸ to resume");
    saveState(true, true);
  } else if (state === "paused") {
    state = "running";
    hideOverlay();
  }
}

// ---------- overlay ----------
function showOverlay(title, msg) {
  $("overlay-title").textContent = title;
  $("overlay-msg").textContent = msg;
  $("overlay").classList.remove("hidden");
}
function hideOverlay() {
  $("overlay").classList.add("hidden");
  $("score-form").classList.add("hidden");
}

// ---------- rendering ----------
function roundRect(x, y, w, h, r) {
  ctx.beginPath();
  ctx.roundRect(x, y, w, h, r);
  ctx.fill();
}

function draw() {
  // background
  ctx.fillStyle = "#0d1220";
  ctx.fillRect(0, 0, canvas.width, canvas.height);

  // subtle grid
  ctx.strokeStyle = "rgba(255,255,255,0.04)";
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let x = 1; x < COLS; x++) { ctx.moveTo(x * CELL, 0); ctx.lineTo(x * CELL, canvas.height); }
  for (let y = 1; y < ROWS; y++) { ctx.moveTo(0, y * CELL); ctx.lineTo(canvas.width, y * CELL); }
  ctx.stroke();

  // walls (sprite tile, primitive fallback)
  for (const w of walls) {
    const [x, y] = w.split(",").map(Number);
    if (spritesOk) {
      ctx.drawImage(sprites.wall, x * CELL, y * CELL, CELL, CELL);
    } else {
      ctx.fillStyle = "#3a4566";
      roundRect(x * CELL + 1, y * CELL + 1, CELL - 2, CELL - 2, 4);
    }
  }

  // glowing food (sprite includes its own glow halo)
  if (food) {
    if (spritesOk) {
      ctx.drawImage(sprites.food, food.x * CELL - 2, food.y * CELL - 2, CELL + 4, CELL + 4);
    } else {
      const fx = food.x * CELL + CELL / 2, fy = food.y * CELL + CELL / 2;
      ctx.save();
      ctx.shadowColor = "#ff5470";
      ctx.shadowBlur = 14;
      ctx.fillStyle = "#ff5470";
      ctx.beginPath();
      ctx.arc(fx, fy, CELL / 2 - 4, 0, Math.PI * 2);
      ctx.fill();
      ctx.restore();
    }
  }

  // body segments (tail first so the head paints on top)
  for (let i = snake.length - 1; i >= 1; i--) {
    const s = snake[i];
    const t = i / Math.max(snake.length - 1, 1);
    ctx.fillStyle = `rgba(76, 217, 140, ${1 - t * 0.6})`;
    roundRect(s.x * CELL + 1.5, s.y * CELL + 1.5, CELL - 3, CELL - 3, 5);
  }

  // head: rotatable sprite (drawn facing right), primitive fallback
  if (snake.length) {
    const h = snake[0];
    if (spritesOk) {
      const angle = Math.atan2(dir.y, dir.x);   // right=0, down=PI/2, ...
      ctx.save();
      ctx.translate(h.x * CELL + CELL / 2, h.y * CELL + CELL / 2);
      ctx.rotate(angle);
      ctx.drawImage(sprites.head, -CELL / 2, -CELL / 2, CELL, CELL);
      ctx.restore();
    } else {
      ctx.fillStyle = "#7cf7a4";
      roundRect(h.x * CELL + 1.5, h.y * CELL + 1.5, CELL - 3, CELL - 3, 7);
    }
  }
}

// ---------- input ----------
const DIRS = {
  up: { x: 0, y: -1 }, down: { x: 0, y: 1 },
  left: { x: -1, y: 0 }, right: { x: 1, y: 0 }
};

function steer(d) {
  const nd = DIRS[d];
  if (!nd || (nd.x === -dir.x && nd.y === -dir.y)) return; // no 180° turns
  nextDir = nd;
}

document.addEventListener("keydown", (e) => {
  const map = {
    ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right",
    w: "up", s: "down", a: "left", d: "right",
    W: "up", S: "down", A: "left", D: "right"
  };
  if (map[e.key]) { e.preventDefault(); steer(map[e.key]); }
  else if (e.key === " ") {
    e.preventDefault();
    if (state === "idle" || state === "over") return; // overlay button handles start
    togglePause();
  }
});

document.querySelectorAll("#pad [data-dir]").forEach((btn) => {
  btn.addEventListener("pointerdown", (e) => { e.preventDefault(); steer(btn.dataset.dir); });
});
$("pause-btn").addEventListener("click", togglePause);
$("btn-start").addEventListener("click", startGame);

$("score-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const player = $("player-name").value.trim() || "Anonymous";
  localStorage.setItem("snake.player", player);
  const { rank } = await uapp.call("submit_score", { player, score, level: levelIdx + 1 });
  $("score-form").classList.add("hidden");
  const res = $("save-result");
  res.textContent = `Saved! You're #${rank} on the board.`;
  res.classList.remove("hidden");
});

// ---------- init ----------
uapp.ready.then(async () => {
  const resp = await fetch("levels.json");   // stored app file
  levels = (await resp.json()).levels;
  $("level-name").textContent = levels[0].name;
  await loadSprites();                       // wait for sprites before the loop can start

  // Offer to resume an autosaved live game.
  const saved = await readSavedState();
  if (saved && saved.alive && Array.isArray(saved.snake) && levels[saved.levelIdx]) {
    const btn = $("btn-resume");
    btn.classList.remove("hidden");
    btn.addEventListener("click", () => resumeGame(saved));
    $("overlay-msg").textContent =
      `Saved game found: score ${saved.score}, level ${saved.levelIdx + 1}`;
  }

  await renderLeaderboard();
  uapp.onChange(renderLeaderboard);          // live-update leaderboard
  draw();
});
