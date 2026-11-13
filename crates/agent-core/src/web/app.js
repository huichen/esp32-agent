/* The board's console.
 *
 * The page is a conversation. Everything else — devices, tools, manifest, schedule,
 * logs, settings — is a sheet behind the menu button, because a phone is the target and
 * a chat plus a bottom composer is the only layout that survives a keyboard.
 *
 * Auth: with a portal PIN set, every call carries the token from POST /api/auth. A 401
 * anywhere opens the login sheet and the failed request is retried after unlocking.
 */
"use strict";

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

const S = {
  token: localStorage.getItem("esp32-agent.token") || "",
  attach: null, // {name, mime, data_b64, kb} — one per message, downscaled here
  // Two counters that are not the same number space: the transcript is indexed by
  // message, the event journal by event. Feeding the transcript's index to
  // `?after=` skipped every event of the run — which is exactly what "no streaming"
  // looked like.
  lastMsg: 0,
  lastEvent: null, // seed the journal head before sending, never replay the old conversation
  rendered: 0,
  historyRevision: 0, // invalidate reads that were started before a conversation change
  stream: null,
  busy: false,
  sending: false,
  changingConversation: false,
  panel: "status",
  afterUnlock: null,
  unlockPromise: null,
  /** Bumped every time the board starts a turn, so a queued message can tell "not yet"
   * from "started and finished" without guessing from timers. */
  turnStarted: 0,
};

/** The timer behind the "queued but not started" note; one at a time. */
let sendTimer = null;

// ---------------------------------------------------------------- transport

async function api(path, opts = {}) {
  const headers = { ...(opts.headers || {}) };
  if (S.token) headers.Authorization = "Bearer " + S.token;
  let body = opts.body;
  if (body !== undefined && typeof body !== "string") {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(body);
  }
  const res = await fetch(path, { method: opts.method || "GET", headers, body });
  if (res.status === 401) {
    const retried = await unlock();
    if (retried) return api(path, opts);
    throw new Error("PIN required");
  }
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { data = { raw: text }; }
  if (!res.ok) {
    const error = new Error((data && data.error) || res.status + " " + res.statusText);
    error.status = res.status;
    throw error;
  }
  return data;
}

function unlock() {
  // Startup reads can be refused together. All of them wait for one login; a
  // second 401 must not strand the first request by replacing its resolver.
  if (S.unlockPromise) return S.unlockPromise;
  S.unlockPromise = new Promise((resolve) => {
    S.afterUnlock = resolve;
    $("#login").hidden = false;
    setTimeout(() => $("#login-pin").focus(), 60);
  });
  return S.unlockPromise;
}

async function doLogin() {
  const pin = $("#login-pin").value.trim();
  const error = $("#login-error");
  error.textContent = "";
  try {
    const res = await fetch("/api/auth", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ pin }),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) {
      error.textContent = data.error || "wrong PIN";
      return;
    }
    S.token = data.token || "";
    if (S.token) localStorage.setItem("esp32-agent.token", S.token);
    $("#login").hidden = true;
    $("#login-pin").value = "";
    const wait = S.afterUnlock;
    S.afterUnlock = null;
    S.unlockPromise = null;
    if (wait) wait(true);
    else location.reload();
  } catch (e) {
    error.textContent = String(e.message || e);
  }
}

function toast(text) {
  const el = $("#toast");
  el.textContent = text;
  el.classList.add("show");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => el.classList.remove("show"), 2200);
}

// ---------------------------------------------------------------- markdown

const escapeHtml = (s) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

/** Only passive links are clickable. HTML escaping alone does not neutralize
 * javascript: or browser-normalized control characters inside a URL. */
function markdownLink(label, escapedTarget) {
  const decoder = document.createElement("textarea");
  decoder.innerHTML = escapedTarget;
  const target = decoder.value.trim();
  if (!target || /[\u0000-\u0020\u007f]/.test(target)) return label;
  try {
    const url = new URL(target, location.href);
    if (!["http:", "https:", "mailto:"].includes(url.protocol)) return label;
  } catch {
    return label;
  }
  return '<a href="' + escapeHtml(target) + '" target="_blank" rel="noopener noreferrer">' + label + "</a>";
}

/** Inline spans, applied to text that has already been HTML-escaped. */
function inline(text) {
  return text
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_, label, target) => markdownLink(label, target));
}

/** One table row's cells. `\|` is a pipe in the text, not a separator. */
function tableRow(line) {
  let s = line.trim();
  if (s.startsWith("|")) s = s.slice(1);
  if (s.endsWith("|")) s = s.slice(0, -1);
  const cells = [];
  let cur = "";
  for (let i = 0; i < s.length; i += 1) {
    if (s[i] === "\\" && s[i + 1] === "|") {
      cur += "|";
      i += 1;
      continue;
    }
    if (s[i] === "|") {
      cells.push(cur.trim());
      cur = "";
      continue;
    }
    cur += s[i];
  }
  cells.push(cur.trim());
  return cells;
}

/** A GFM table: `| a | b |`, then `|---|---|`, then the rows.
 *
 * The model emits tables — a device list is four columns and that is the shape it reaches
 * for — and a phone is where raw pipes look worst: the columns run together into one
 * paragraph of `|` and `---`. The wrapper scrolls sideways rather than letting a wide table
 * push the bubble off its row. */
function tableFrom(lines, at) {
  const head = lines[at];
  const sep = lines[at + 1];
  if (!head.includes("|") || sep === undefined || !sep.includes("|") || !/^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/.test(sep)) {
    return null;
  }
  const header = tableRow(head);
  if (header.length < 2) return null;
  const align = tableRow(sep).map((c) =>
    c.startsWith(":") && c.endsWith(":") ? "center" : c.endsWith(":") ? "right" : c.startsWith(":") ? "left" : ""
  );
  const rows = [];
  let i = at + 2;
  while (i < lines.length && lines[i].includes("|") && lines[i].trim() !== "") {
    rows.push(tableRow(lines[i]));
    i += 1;
  }
  const cell = (tag, text, column) => {
    const a = align[column];
    return "<" + tag + (a ? ' style="text-align:' + a + '"' : "") + ">" + inline(text || "") + "</" + tag + ">";
  };
  const body = rows
    .map((r) => "<tr>" + header.map((_, c) => cell("td", r[c], c)).join("") + "</tr>")
    .join("");
  const html =
    '<div class="table-wrap"><table><thead><tr>' +
    header.map((t, c) => cell("th", t, c)).join("") +
    "</tr></thead>" +
    (body ? "<tbody>" + body + "</tbody>" : "") +
    "</table></div>";
  return { html, next: i };
}

/** A small Markdown subset for model replies, with raw HTML kept as text. */
function markdown(text) {
  const blocks = [];
  let src = escapeHtml(text || "").replace(/```([\s\S]*?)```/g, (_, code) => {
    blocks.push(code.replace(/^\w*\n/, ""));
    return "\u0000BLOCK" + (blocks.length - 1) + "\u0000";
  });

  // Tables come out before the inline rules: a row is a block, and the paragraph wrapper
  // below would make it one <p> of pipes.
  const tables = [];
  {
    const lines = src.split("\n");
    const out = [];
    for (let i = 0; i < lines.length; ) {
      const table = tableFrom(lines, i);
      if (table) {
        tables.push(table.html);
        out.push("\u0000TABLE" + (tables.length - 1) + "\u0000");
        i = table.next;
        continue;
      }
      out.push(lines[i]);
      i += 1;
    }
    src = out.join("\n");
  }

  src = src
    .replace(/^### (.*)$/gm, "<h3>$1</h3>")
    .replace(/^## (.*)$/gm, "<h2>$1</h2>")
    .replace(/^# (.*)$/gm, "<h1>$1</h1>")
    .replace(/^&gt; (.*)$/gm, "<blockquote>$1</blockquote>")
    .replace(/^[-*] (.*)$/gm, "<li>$1</li>");
  src = inline(src);

  // Wrap runs of list items, then paragraphs.
  src = src.replace(/(?:<li>.*<\/li>\n?)+/g, (m) => "<ul>" + m.replace(/\n/g, "") + "</ul>");
  const html = src
    .split(/\n{2,}/)
    .map((chunk) => {
      const t = chunk.trim();
      if (!t) return "";
      if (/^(?:<(?:h\d|ul|blockquote|pre|p)\b|\u0000(?:BLOCK|TABLE))/.test(t)) return t;
      return "<p>" + t.replace(/\n/g, "<br>") + "</p>";
    })
    .join("");
  return html
    .replace(/\u0000BLOCK(\d+)\u0000/g, (_, i) => "<pre><code>" + blocks[Number(i)] + "</code></pre>")
    .replace(/\u0000TABLE(\d+)\u0000/g, (_, i) => tables[Number(i)]);
}

// ---------------------------------------------------------------- chat view

function addRow(who) {
  const row = document.createElement("div");
  row.className = "row " + who;
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  row.appendChild(bubble);
  $("#chat").appendChild(row);
  scrollToEnd();
  return { row, bubble };
}

/** A tool argument or a tool result, laid out so a person can read it.
 *
 * The board hands these over as JSON text, and the payload people actually read is usually
 * one string field — a script's source, a shell's `stdout`, a tool's `text` — with its
 * newlines escaped. Printing the JSON as-is is the wall of text with `\n` in it that the
 * console used to show. So: decode that field into a block of its own and put the rest of
 * the object on one line after it. Everything else gets two-space JSON, and anything that
 * is not JSON is left exactly as it came — a tool result is not always structured. */
const PROSE_FIELDS = ["text", "output", "stdout", "stderr", "message", "error", "detail", "source"];

function pretty(text) {
  if (text == null) return "";
  const raw = String(text);
  const trimmed = raw.trim();
  if (!/^[[{]/.test(trimmed)) return raw;
  let value;
  try {
    value = JSON.parse(trimmed);
  } catch {
    return raw;
  }
  if (value && typeof value === "object" && !Array.isArray(value)) {
    const key = PROSE_FIELDS.find((k) => typeof value[k] === "string" && value[k].includes("\n"));
    if (key) {
      const rest = Object.keys(value)
        .filter((k) => k !== key)
        .map((k) => {
          const v = value[k];
          return k + ": " + (typeof v === "string" ? v : JSON.stringify(v));
        })
        .join(", ");
      return value[key] + (rest ? "\n\n— " + rest : "");
    }
  }
  return JSON.stringify(value, null, 2);
}

function chip(tools, label, detail) {
  // A tool call reads as one compact line; the arguments and the result live behind the
  // disclosure triangle. A chat that shows every result inline is a log, not a chat.
  const wrap = document.createElement("details");
  wrap.className = "tool";
  const summary = document.createElement("summary");
  summary.textContent = label;
  wrap.appendChild(summary);
  for (const [name, text] of detail || []) {
    const pre = document.createElement("pre");
    pre.textContent = name + ": " + pretty(text);
    wrap.appendChild(pre);
  }
  tools.appendChild(wrap);
  return wrap;
}

/** Mark a tool row green or red once its result arrives. */
function finishChip(wrap, ok) {
  if (!wrap) return;
  wrap.classList.add(ok ? "ok" : "err");
}

function scrollToEnd() {
  const chat = $("#chat");
  chat.scrollTop = chat.scrollHeight;
}

/** The header dot: `live` answered and idle, `busy` a run is in flight, `off` no answer. */
function setLink(state) {
  const el = $("#link");
  if (el) el.className = "dot " + state;
}

/** One turn: a user message and everything the board did while answering it.
 *
 * The transcript shows the answer. The work that produced it — tool calls, intermediate
 * notes, the model's thinking — collapses into one line above it, the way a chat should
 * read. Nothing is hidden: the line opens. */
function groupTurns(msgs) {
  const turns = [];
  for (const m of msgs) {
    if (m.role === "user") {
      turns.push({ user: m, entries: [] });
      continue;
    }
    if (!turns.length) turns.push({ user: null, entries: [] });
    turns[turns.length - 1].entries.push(m);
  }
  return turns;
}

/** Split one turn into the answer plus the trace that led to it. */
function splitTurn(entries) {
  const results = new Map();
  for (const m of entries) if (m.role === "tool") results.set(m.tool_call_id, m);

  // The answer is the last thing the board said in words; everything before it is trace.
  const spoken = entries.filter((m) => m.role !== "tool" && (m.text || "").trim());
  const last = spoken.length ? spoken[spoken.length - 1] : null;

  let thinking = "";
  const steps = [];
  for (const m of entries) {
    if (m.role === "tool") continue;
    if (m.reasoning) thinking += (thinking ? "\n\n" : "") + m.reasoning;
    const text = (m.text || "").trim();
    if (text && m !== last) steps.push({ kind: "say", text });
    for (const call of m.tool_calls || []) {
      steps.push({ kind: "call", call, result: results.get(call.id) });
    }
  }
  return { thinking, steps, answer: last ? last.text : "" };
}

function previewArgs(args) {
  let text = typeof args === "string" ? args : JSON.stringify(args || {});
  if (!text || text === "{}") return "";
  text = text.replace(/\s+/g, " ");
  return text.length > 42 ? text.slice(0, 41) + "\u2026" : text;
}

function counter(n, one, many) {
  return n + " " + (n === 1 ? one : many);
}

/** The collapsed line above an answer: how much work, and the work itself. */
function renderTrace(thinking, steps) {
  const row = document.createElement("div");
  row.className = "row trace";
  const box = document.createElement("details");
  box.className = "trace-box";

  // The summary is a glyph and a count per kind, the way FutureOS folds a step run:
  // the line has to survive in a phone's margin, and the words live in the title.
  const calls = steps.filter((s) => s.kind === "call").length;
  const said = steps.length - calls;
  const counts = [];
  if (calls) counts.push("\u{1F527} " + calls);
  if (said) counts.push("\u270E " + said);
  if (thinking) counts.push("\u{1F9E0}");
  const summary = document.createElement("summary");
  summary.textContent = counts.join(" \u00b7 ") || "details";
  summary.title = [
    calls ? counter(calls, "tool call", "tool calls") : "",
    said ? counter(said, "intermediate message", "intermediate messages") : "",
    thinking ? "the model's thinking" : "",
  ]
    .filter(Boolean)
    .join(" \u00b7 ");
  box.appendChild(summary);

  if (thinking) {
    const inner = document.createElement("details");
    inner.className = "think";
    const s2 = document.createElement("summary");
    s2.textContent = "thinking";
    inner.appendChild(s2);
    const body = document.createElement("div");
    body.className = "think-text";
    body.innerHTML = markdown(thinking);
    inner.appendChild(body);
    box.appendChild(inner);
  }

  for (const step of steps) {
    if (step.kind === "say") {
      const div = document.createElement("div");
      div.className = "step-text";
      div.innerHTML = markdown(step.text);
      box.appendChild(div);
      continue;
    }
    const call = step.call.function || step.call;
    const d = document.createElement("details");
    d.className = "tool";
    const sum = document.createElement("summary");
    sum.textContent = call.name + "(" + previewArgs(call.arguments) + ")";
    d.appendChild(sum);
    // The same `pretty()` the live chip uses: a restored transcript must read like the one
    // that produced it, and a tool result is a JSON document with newlines escaped inside a
    // string, which printed raw is one unreadable line.
    const args = document.createElement("pre");
    args.textContent = "arguments: " + pretty(typeof call.arguments === "string" ? call.arguments : JSON.stringify(call.arguments || {}));
    if (call.arguments_elided_bytes) {
      // Said rather than hidden: a cut document that looks whole is worse than none.
      args.textContent += "\n... " + call.arguments_elided_bytes + " more bytes not kept";
    }
    d.appendChild(args);
    if (step.result) {
      const res = document.createElement("pre");
      res.textContent = "result: " + pretty(step.result.text || "");
      d.appendChild(res);
      if (step.result.is_error) d.classList.add("err");
      else d.classList.add("ok");
    }
    box.appendChild(d);
  }

  row.appendChild(box);
  $("#chat").appendChild(row);
  scrollToEnd();
}

function renderTurn(turn) {
  if (turn.user) {
    const { bubble } = addRow("user");
    bubble.innerHTML = markdown(turn.user.text || "");
  }
  const { thinking, steps, answer } = splitTurn(turn.entries);
  if (thinking || steps.length) renderTrace(thinking, steps);
  if (answer) {
    const { bubble } = addRow("agent");
    bubble.innerHTML = markdown(answer);
  }
}

function renderHistory(data) {
  $("#chat").innerHTML = "";
  const msgs = (data && data.messages) || [];
  S.rendered = msgs.length;
  S.lastMsg = 0;
  for (const m of msgs) S.lastMsg = Math.max(S.lastMsg, m.seq || 0);
  if (!msgs.length) {
    const { bubble } = addRow("agent");
    bubble.innerHTML =
      "<p>This is the board's own agent. Ask it about the hardware, tell it to change something, or give it a job &mdash; it calls its tools by itself.</p>";
    return;
  }
  for (const turn of groupTurns(msgs)) renderTurn(turn);
  scrollToEnd();
}

/** The one thing that draws the pull hint, so its words and its spinner cannot disagree. */
function setPullHint(state, text) {
  const hint = $("#pull-hint");
  if (text) $("#pull-label").textContent = text;
  hint.classList.toggle("show", state !== "idle");
  hint.classList.toggle("refreshing", state === "refreshing");
}

/** Reload what the board has, without touching the draft or the settings.
 *
 * This is what the pull gesture at the end of the transcript asks for: the same history
 * read and cursor sync a reload would do, minus a page refresh that would discard a
 * half-typed message. The hint stays up with a turning ring while it runs, because a
 * request that takes a second and shows nothing reads as a dead page. */
async function refreshConversation() {
  if (S.changingConversation || S.sending) return;
  const started = performance.now();
  setPullHint("refreshing", "Refreshing\u2026");
  let ok = true;
  try {
    ok = await loadHistory(true);
  } catch {
    ok = false;
  }
  if (ok) {
    try { await syncEventCursor(); } catch { /* the next send retries it */ }
    // The transcript is what was asked for; a status read that fails must not swallow the
    // confirmation.
    try { await loadStatus(); } catch { /* offline or busy: the history is still reloaded */ }
  }
  // A ring that appears and vanishes inside one frame reads as a glitch rather than as work.
  const spent = performance.now() - started;
  if (spent < 300) await new Promise((resolve) => setTimeout(resolve, 300 - spent));
  setPullHint("idle", "Pull up to refresh");
  toast(ok ? "Conversation refreshed" : "Refresh failed; the conversation was not reloaded");
}

/** At the end of the transcript, scrolling further has nowhere to go: the page bounced and a
 * person could not tell "the board has nothing more" from "the page is stuck". Past the last
 * message a drag *upwards* — which is the motion that scrolls the conversation further
 * forward — offers a refresh, which reloads the transcript in place. */
function wirePullToRefresh() {
  const chat = $("#chat");
  const THRESHOLD = 60;
  let startY = null;
  let overshoot = 0;
  const atEnd = () => chat.scrollTop + chat.clientHeight >= chat.scrollHeight - 4;
  chat.addEventListener("touchstart", (event) => {
    startY = atEnd() && event.touches.length === 1 ? event.touches[0].clientY : null;
    overshoot = 0;
  }, { passive: true });
  chat.addEventListener("touchmove", (event) => {
    if (startY === null) return;
    // Positive when the finger moves up, which is the gesture that would have kept scrolling
    // down. The old sign measured a downward drag, so it took a scroll *backwards* to refresh.
    overshoot = startY - event.touches[0].clientY;
    if (overshoot > 12) {
      setPullHint("pulling", overshoot >= THRESHOLD ? "Release to refresh" : "Pull up to refresh");
    } else {
      setPullHint("idle");
    }
  }, { passive: true });
  const finish = () => {
    const shouldRefresh = startY !== null && overshoot >= THRESHOLD;
    startY = null;
    overshoot = 0;
    // `refreshConversation` owns the hint from here: clearing it first would blank the
    // spinner for the frame between the gesture and the request.
    if (shouldRefresh) refreshConversation();
    else setPullHint("idle");
  };
  chat.addEventListener("touchend", finish);
  chat.addEventListener("touchcancel", finish);
}

async function loadHistory(duringChange = false) {
  if (S.changingConversation && !duringChange) return false;
  const revision = ++S.historyRevision;
  try {
    const data = await api("/api/agent/history");
    if (revision !== S.historyRevision || (S.changingConversation && !duringChange)) return false;
    renderHistory(data);
    return true;
  } catch (e) {
    if (revision === S.historyRevision && !String(e.message).includes("PIN")) toast("history: " + e.message);
    return false;
  }
}

/** Polls and explicit reloads share the same read fence: a response captured before a
 * clear must not redraw the old transcript, even if it arrives after the clear finished. */
async function pollHistory() {
  if (S.busy || S.stream || S.changingConversation || S.sending) return;
  const revision = S.historyRevision;
  try {
    const data = await api("/api/agent/history");
    if (revision !== S.historyRevision || S.busy || S.stream || S.changingConversation || S.sending) return;
    const count = ((data && data.messages) || []).length;
    if (count !== S.rendered) await loadHistory();
  } catch {
    /* offline or locked: the next tick tries again */
  }
}

/** The event journal can outlive the transcript, including a Memory forget. Read its
 * head instead of guessing from message indices or replaying from zero. If this read
 * fails after a clear, the next send retries it before submitting a new turn. */
async function syncEventCursor() {
  const data = await api("/api/agent/events?after=" + (S.lastEvent || 0));
  if (!data || !Number.isSafeInteger(data.seq) || data.seq < 0) throw new Error("invalid event cursor");
  S.lastEvent = data.seq;
}

const CONVERSATION_TEXT = {
  confirm: "This clears the conversation and its compressed history. Instructions, skills, files and model settings stay.",
  busy: "The agent is working. Wait for this turn to finish; clearing does not cancel it.",
  failed: "Could not change the conversation. History was kept.",
  cleared: "Conversation cleared.",
  kept: "Kept the last exchange.",
  refreshFailed: "Conversation changed, but the refresh failed. Reload the page to retry.",
};

/** Our own confirmation, because window.confirm() draws its title and buttons in the
 * device's language: an English page grew Chinese buttons on a Chinese phone. Resolves
 * true for the action button, false for cancel, Escape or a tap outside the card. */
function askConfirm(title, body, okLabel) {
  return new Promise((resolve) => {
    const dialog = $("#confirm-dialog");
    const ok = $("#confirm-ok");
    const cancel = $("#confirm-cancel");
    const done = (answer) => {
      dialog.hidden = true;
      dialog.removeEventListener("keydown", onKey);
      ok.onclick = null;
      cancel.onclick = null;
      dialog.onclick = null;
      resolve(answer);
    };
    const onKey = (event) => {
      if (event.key === "Escape") { event.preventDefault(); done(false); }
      else if (event.key === "Enter") { event.preventDefault(); done(true); }
    };
    $("#confirm-title").textContent = title;
    $("#confirm-body").textContent = body;
    ok.textContent = okLabel;
    ok.onclick = () => done(true);
    cancel.onclick = () => done(false);
    dialog.onclick = (event) => { if (event.target === dialog) done(false); };
    dialog.addEventListener("keydown", onKey);
    dialog.hidden = false;
    cancel.focus();
  });
}

function conversationControls() {
  for (const id of ["#clear-chat", "#forget-tail", "#forget-all"]) {
    $(id).disabled = S.changingConversation;
    $(id).setAttribute("aria-busy", String(S.changingConversation));
  }
  $("#send").disabled = S.changingConversation || S.sending;
}

/** Both the chat-header clear and the Memory controls use this path. A refused POST
 * changes nothing visible and leaves a running stream alone: reset is not cancellation. */
async function changeConversation(path, body, clearAll) {
  if (S.changingConversation) return;
  if (S.busy || S.sending) { toast(CONVERSATION_TEXT.busy); return; }
  if (clearAll && !(await askConfirm("Clear conversation", CONVERSATION_TEXT.confirm, "Clear"))) return;
  S.changingConversation = true;
  ++S.historyRevision;
  conversationControls();
  try {
    await api(path, { method: "POST", body });
  } catch (e) {
    toast(e.status === 409 ? CONVERSATION_TEXT.busy : CONVERSATION_TEXT.failed + "\n" + e.message);
    S.changingConversation = false;
    conversationControls();
    return;
  }

  // Only a successful mutation retires the old stream and its live DOM references.
  stopStream();
  hideLiveNote();
  liveTrace = null;
  liveBubble = null;
  liveCalls = 0;
  S.busy = false;
  S.lastEvent = null;
  S.lastMsg = 0;
  S.rendered = 0;
  $("#chat").innerHTML = "";
  if (clearAll) renderHistory({ messages: [] });
  const [historyLoaded, cursorSynced] = await Promise.all([
    loadHistory(true),
    syncEventCursor().then(() => true, () => false),
  ]);
  S.changingConversation = false;
  conversationControls();
  toast(historyLoaded && cursorSynced
    ? (clearAll ? CONVERSATION_TEXT.cleared : CONVERSATION_TEXT.kept)
    : CONVERSATION_TEXT.refreshFailed);
  if (!$("#sheet").hidden && S.panel === "agent") loadAgent();
}

/** Downscale an image in the browser, before it crosses the LAN.
 *
 * The board keeps an attachment in RAM and a request has to fit what its TLS stack can
 * send, so a 4 MB phone photo is not a thing to upload: 1024 px on the long side as JPEG
 * is ~20 kB and still legible. If it is still too big after that, say so instead of
 * handing the board a request it will refuse. */
/** The board's attachment ceiling, in decoded bytes. Keep in step with
 * `MAX_ATTACHMENT_BYTES` in app.rs: the board is the authority, and a refusal after the
 * upload is the worst place to find out. */
const ATTACH_BYTES_MAX = 24 * 1024;

/** Decoded size of a base64 payload, exactly: four characters carry three bytes and the
 * `=` padding is not data. `length * 0.75` was close enough to look right and wrong enough
 * to send 26 328 bytes into a 24 576-byte limit. */
function base64Bytes(b64) {
  const padding = b64.endsWith("==") ? 2 : b64.endsWith("=") ? 1 : 0;
  return Math.floor((b64.length * 3) / 4) - padding;
}

function downscale(file) {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      const canvas = document.createElement("canvas");
      const ctx = canvas.getContext("2d");
      // Shrink, then give up quality, and stop at the first size that fits. The old code
      // stepped quality down once and never re-measured, so a 20 MP photo could pass the
      // check and still be refused by the board.
      const steps = [
        { edge: 1024, quality: 0.72 },
        { edge: 1024, quality: 0.55 },
        { edge: 768, quality: 0.62 },
        { edge: 640, quality: 0.6 },
        { edge: 512, quality: 0.55 },
        { edge: 384, quality: 0.5 },
        { edge: 256, quality: 0.5 },
      ];
      let best = null;
      for (const step of steps) {
        const scale = Math.min(1, step.edge / Math.max(img.width, img.height));
        const w = Math.max(1, Math.round(img.width * scale));
        const h = Math.max(1, Math.round(img.height * scale));
        canvas.width = w;
        canvas.height = h;
        ctx.drawImage(img, 0, 0, w, h);
        const dataUrl = canvas.toDataURL("image/jpeg", step.quality);
        const data_b64 = dataUrl.split(",", 2)[1] || "";
        best = { w, h, data_b64, bytes: base64Bytes(data_b64) };
        if (best.bytes <= ATTACH_BYTES_MAX) break;
      }
      if (!best || best.bytes > ATTACH_BYTES_MAX) {
        reject(new Error(
          "that image is " + Math.ceil(best.bytes / 1024) + " kB even at " + best.w + "\u00d7" + best.h +
          "; the board accepts " + ATTACH_BYTES_MAX / 1024 + " kB — crop it or pick a smaller one"));
        return;
      }
      resolve({
        name: file.name || "image.jpg",
        mime: "image/jpeg",
        data_b64: best.data_b64,
        kb: Math.max(1, Math.round(best.bytes / 1024)),
      });
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("could not read that image"));
    };
    img.src = url;
  });
}

function showAttach() {
  const chip = $("#attach-chip");
  if (!S.attach) {
    chip.hidden = true;
    chip.innerHTML = "";
    return;
  }
  chip.hidden = false;
  chip.innerHTML =
    '<span class="attach-name">' + escapeHtml(S.attach.name) + " \u00b7 " + S.attach.kb + " kB</span>" +
    '<button type="button" class="attach-x" aria-label="Remove attachment">\u00d7</button>';
  chip.querySelector(".attach-x").onclick = () => {
    S.attach = null;
    showAttach();
  };
}

async function send(text) {
  const trimmed = text.trim();
  if (!trimmed || S.changingConversation || S.sending) return;
  S.sending = true;
  conversationControls();
  const attachment = S.attach;
  const shown = trimmed + (attachment ? "\n\n_[image: " + attachment.name + "]_" : "");
  let bubble = null;
  try {
    if (S.lastEvent === null) await syncEventCursor();
    bubble = addRow("user").bubble;
    bubble.innerHTML = markdown(shown);
    $("#input").value = "";
    autosize();
    S.attach = null;
    showAttach();
    const payload = { text: trimmed };
    if (attachment) payload.attachments = [attachment];
    await postMessage(payload);
    S.busy = true;
    $("#link").className = "dot busy";
    liveTrace = null;
    liveBubble = null;
    liveCalls = 0;
    connectStream();
    watchTurnStart();
  } catch (e) {
    // The bubble stays — a question that was typed should not vanish — but it says it was
    // not delivered, the draft comes back, and the attachment is put back so a retry does
    // not need the file picker again.
    if (bubble) {
      bubble.classList.add("failed");
      bubble.innerHTML = markdown(shown);
    }
    if (!$("#input").value.trim()) {
      $("#input").value = trimmed;
      autosize();
    }
    if (attachment && !S.attach) {
      S.attach = attachment;
      showAttach();
    }
    toast(e.message);
  } finally {
    S.sending = false;
    conversationControls();
  }
}

/** One POST with a deadline.
 *
 * The board answers 202 as soon as it queues, but while it is busy the request can sit in
 * the socket for minutes — and a fetch with no deadline looks exactly like a delivered
 * message. Twenty seconds is longer than a queue admission should ever take. */
async function postMessage(payload) {
  let timer = null;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(
      "the board did not answer within 20 s; the message may not have arrived")), 20000);
  });
  try {
    return await Promise.race([api("/api/agent/message", { method: "POST", body: payload }), deadline]);
  } finally {
    if (timer !== null) clearTimeout(timer);
  }
}

/** Say something when a queued message has produced no run.
 *
 * A message can be accepted while the board is still finishing the previous turn; the page
 * then looks dead for as long as that takes. Nothing here cancels or resends — it only
 * stops the silence from being read as "the board never got it". */
function watchTurnStart() {
  if (sendTimer !== null) clearTimeout(sendTimer);
  const before = S.turnStarted;
  sendTimer = setTimeout(() => {
    sendTimer = null;
    if (S.turnStarted === before && !S.changingConversation) {
      toast("Queued. The board has not started this turn yet — it may still be working on the previous one.");
    }
  }, 20000);
}

// ---------------------------------------------------------------- streaming

let liveTrace = null;
let liveBubble = null;
let liveCalls = 0;
let liveNote = null;

/** The run in progress: a trace that grows, and a bubble for the text it streams. */
function ensureLiveTrace() {
  if (liveTrace) return liveTrace;
  const row = document.createElement("div");
  row.className = "row trace";
  const box = document.createElement("details");
  box.className = "trace-box live";
  const summary = document.createElement("summary");
  summary.textContent = "working\u2026";
  box.appendChild(summary);
  row.appendChild(box);
  $("#chat").appendChild(row);
  liveTrace = box;
  scrollToEnd();
  return box;
}

function updateLiveSummary() {
  if (!liveTrace) return;
  const summary = liveTrace.querySelector("summary");
  if (!summary) return;
  summary.textContent = liveCalls
    ? counter(liveCalls, "tool call", "tool calls") + " \u00b7 running"
    : "working\u2026";
  if (liveNote) {
    liveNote.textContent = liveCalls ? counter(liveCalls, "tool call", "tool calls") + " \u2026" : "working\u2026";
  }
}

/** A row with moving dots, between the last message and whatever comes next.
 *
 * A model call takes seconds and the board sends nothing until it has an answer, so the
 * page has to say "still working" on its own: without it a long turn is indistinguishable
 * from a page that stopped listening. It is not a transcript entry -`run_end` removes it. */
function showLiveNote() {
  if (liveNote) return;
  const row = document.createElement("div");
  row.className = "row agent live-row";
  const note = document.createElement("div");
  note.className = "live-note";
  for (let i = 0; i < 3; i += 1) {
    const dot = document.createElement("span");
    dot.className = "dot-anim";
    note.appendChild(dot);
  }
  const label = document.createElement("span");
  label.className = "live-label";
  label.textContent = "working\u2026";
  note.appendChild(label);
  row.appendChild(note);
  $("#chat").appendChild(row);
  liveNote = label;
  scrollToEnd();
}

function hideLiveNote() {
  if (!liveNote) return;
  const row = liveNote.closest(".live-row");
  if (row) row.remove();
  liveNote = null;
}

function ensureLiveBubble() {
  if (liveBubble) return liveBubble;
  const { bubble } = addRow("agent");
  bubble.dataset.raw = "";
  liveBubble = bubble;
  return bubble;
}

function handleEvent(name, data) {
  if (data.seq && S.lastEvent !== null && data.seq <= S.lastEvent) return;
  S.lastEvent = Math.max(S.lastEvent || 0, data.seq || 0);
  switch (name) {
    case "message_start":
      S.turnStarted += 1;
      if (sendTimer !== null) { clearTimeout(sendTimer); sendTimer = null; }
      S.busy = true;
      liveTrace = null;
      liveBubble = null;
      liveCalls = 0;
      $("#link").className = "dot busy";
      showLiveNote();
      break;
    case "thinking_delta": {
      const box = ensureLiveTrace();
      let inner = box.querySelector("details.think");
      if (!inner) {
        inner = document.createElement("details");
        inner.className = "think";
        inner.open = true;
        const sum = document.createElement("summary");
        sum.textContent = "thinking";
        inner.appendChild(sum);
        const body = document.createElement("div");
        body.className = "think-text";
        inner.appendChild(body);
        box.appendChild(inner);
      }
      const body = inner.querySelector(".think-text");
      body.dataset.raw = (body.dataset.raw || "") + (data.text || "");
      body.innerHTML = markdown(body.dataset.raw);
      scrollToEnd();
      break;
    }
    case "text_delta": {
      const bubble = ensureLiveBubble();
      bubble.dataset.raw = (bubble.dataset.raw || "") + (data.text || "");
      bubble.innerHTML = markdown(bubble.dataset.raw);
      scrollToEnd();
      break;
    }
    case "tool_use": {
      const box = ensureLiveTrace();
      liveCalls += 1;
      updateLiveSummary();
      // The summary carries the arguments, as the restored transcript's does: `script()`
      // with empty parens reads as "called with nothing", and that is how a call with a
      // long source was being shown.
      let shown = data.text || "{}";
      try { shown = JSON.stringify(JSON.parse(shown)); } catch (e) { /* keep it as sent */ }
      const wrap = chip(box, (data.name || "tool") + "(" + previewArgs(shown) + ")", [
        ["arguments", data.text || "{}"],
      ]);
      wrap.dataset.id = data.tool_call_id || "";
      break;
    }
    case "tool_result": {
      if (!liveTrace) break;
      const rows = Array.from(liveTrace.querySelectorAll("details.tool"));
      // By id first: the event carries `tool_call_id`, and a turn that calls the same tool
      // twice used to attach every result to the last unanswered chip -so the arguments
      // shown beside a result belonged to a different call than the result did.
      const open = rows.filter((d) => !d.classList.contains("ok") && !d.classList.contains("err"));
      const last =
        (data.tool_call_id && open.find((d) => d.dataset.id === data.tool_call_id)) || open.pop() || rows.pop();
      if (last) {
        const pre = document.createElement("pre");
        pre.textContent = "result: " + (data.text || "");
        last.appendChild(pre);
        finishChip(last, !data.is_error);
      }
      break;
    }
    case "retried": {
      // One line, once per model call: a retry that worked is not a failure, so it goes in
      // the collapsed trace rather than in a toast, and the failed attempts never appear.
      const box = ensureLiveTrace();
      chip(box, "retried", [["why", data.text || "the model request was retried"]]);
      break;
    }
    case "error":
      hideLiveNote();
      toast(data.text || "run failed");
      break;
    case "run_end":
      S.busy = false;
      hideLiveNote();
      liveTrace = null;
      liveBubble = null;
      liveCalls = 0;
      $("#link").className = "dot live";
      // Re-render from the board's own transcript: it is the authority on what belongs in
      // the answer and what belongs in the collapsed trace.
      loadHistory();
      // The run is over: give the board its 8 kB back.
      {
        const stream = S.stream;
        setTimeout(() => { if (!S.busy && S.stream === stream) stopStream(); }, 500);
      }
      break;
  }
}

const EVENT_KINDS = [
  "message_start", "thinking_start", "thinking_delta", "thinking_end",
  "text_start", "text_delta", "text_end", "tool_use", "tool_progress",
  "tool_result", "retried", "usage", "message_end", "error", "run_end",
];

function connectStream() {
  if (S.stream || S.changingConversation || S.lastEvent === null) return;
  // EventSource cannot set request headers. The server accepts only a minted,
  // expiring portal token here (never a raw PIN), and only for reads.
  const es = new EventSource("/api/agent/stream?after=" + S.lastEvent +
    (S.token ? "&token=" + encodeURIComponent(S.token) : ""));
  S.stream = es;
  for (const kind of EVENT_KINDS) {
    es.addEventListener(kind, (ev) => {
      if (S.stream !== es) return; // a queued callback from a retired conversation
      let data = {};
      try { data = JSON.parse(ev.data); } catch { return; }
      handleEvent(kind, data);
    });
  }
  es.onopen = () => { if (S.stream === es) $("#link").className = S.busy ? "dot busy" : "dot live"; };
  // The board closes the stream after 30 s of silence or 120 s total; EventSource would
  // resume with Last-Event-ID, which this server does not read, so reconnect by hand.
  es.onerror = () => {
    if (S.stream !== es) return;
    es.close();
    S.stream = null;
    $("#link").className = "dot off";
    if (S.busy) setTimeout(() => { if (S.busy && !S.changingConversation) connectStream(); }, 1500);
  };
}

/** Close the stream when nothing is running.
 *
 * A stream costs ~8 kB of internal RAM on the board for as long as it is open, and the
 * model call needs that RAM for its TLS handshake: a page that keeps a stream open while it
 * sits idle makes every run start with too little. So the stream follows the run. */
function stopStream() {
  if (!S.stream) return;
  S.stream.close();
  S.stream = null;
}

// ---------------------------------------------------------------- panels

/** In-flight panel reads, and the loader that says so.
 *
 * Every tab that renders board state fetches it on open, and until this existed a slow
 * answer looked exactly like an empty tab: the previous panel's content disappeared, the new
 * one had nothing in it, and nothing said whether that was the truth or a request in flight.
 *
 * The loader waits 150 ms before appearing — a LAN answer that arrives in 20 ms should not
 * flash a spinner — and stays until the last read finishes, so entering Agent (which reads
 * both the model and the schedule) does not hide the loader between the two.
 */
let loadsInFlight = 0;
let loadTimer = null;

function beginPanelLoad(label) {
  loadsInFlight += 1;
  if (loadTimer === null && loadsInFlight === 1) {
    loadTimer = setTimeout(() => {
      loadTimer = null;
      if (loadsInFlight > 0) showPanelLoading(label);
    }, 150);
  }
}

function endPanelLoad() {
  loadsInFlight = Math.max(0, loadsInFlight - 1);
  if (loadsInFlight > 0) return;
  if (loadTimer !== null) {
    clearTimeout(loadTimer);
    loadTimer = null;
  }
  showPanelLoading(null);
}

function showPanelLoading(label) {
  const box = $("#panel-loading");
  if (!box) return;
  if (!label) {
    box.hidden = true;
    return;
  }
  $("#panel-loading-label").textContent = label;
  box.hidden = false;
}

function kv(el, pairs) {
  el.innerHTML = pairs
    .map(([k, v]) => "<span>" + escapeHtml(k) + "</span><span>" + escapeHtml(String(v)) + "</span>")
    .join("");
}

/** Uptime in the unit that makes it readable: seconds under a minute, minutes under an
 * hour, then hours and minutes. "0 min" for the first minute was the entire reading on a
 * board that had just booted — and every flash reboots it. */
function formatUptime(seconds) {
  const total = Math.max(0, Math.floor(Number(seconds) || 0));
  if (total < 60) return total + " s";
  if (total < 3600) return Math.floor(total / 60) + " min";
  return Math.floor(total / 3600) + " h " + String(Math.floor((total % 3600) / 60)).padStart(2, "0") + " min";
}

async function loadStatus() {
  try {
    const d = await api("/api/status");
    kv($("#status-kv"), [
      ["Name", d.device_name],
      ["Profile", d.board.profile],
      ["Chip", d.board.soc + " / " + d.board.cores + " cores"],
      ["Uptime", formatUptime(d.uptime_s)],
      ["Heap", d.memory.free_heap_kb + " kB"],
      ["Devices", d.devices.count],
      ["Tools", d.mcp.tools_visible],
      ["Clock", d.time && d.time.synced ? "synced" : "not synced"],
      ["Internal", (d.memory.internal_kb ?? "?") + " kB (largest " + (d.memory.largest_internal_kb ?? "?") + ")"],
    ]);
    kv($("#wifi-kv"), [
      ["State", d.wifi.state],
      ["SSID", d.wifi.ssid || "-"],
      ["IP", d.wifi.ip || "-"],
      ["AP", d.wifi.ap_ip],
    ]);
    // The model settings live in the Agent panel now; the header line is enough here.
    const model = await api("/api/agent/config");
    // No IP in the header: the address bar already shows where we are, and on a phone
    // that line is the most expensive space on the page.
    $("#sub").textContent =
      d.devices.count + " devices · " + d.mcp.tools_visible + " tools · " + (model.model || "no model");
    $("#title").textContent = d.device_name || "esp32-agent";
    // The dot follows what the page actually knows: a poll that answered, and whether a run is
    // in flight. It used to be set only by the SSE stream, which this page deliberately does
    // not open while idle — so a freshly loaded portal sat grey until the first turn, and a
    // grey dot next to a working board reads as a fault.
    setLink(d.agent && d.agent.pending ? "busy" : "live");
  } catch (e) {
    if (!String(e.message).includes("PIN")) {
      $("#sub").textContent = "offline";
      setLink("off");
    }
  }
}

async function loadDevices() {
  try {
    const d = await api("/api/devices");
    const detail = (dev) =>
      '<details class="item-detail"><summary><b>' + escapeHtml(dev.id) + "</b> <span>" +
      escapeHtml(dev.name) + (dev.address ? " @ " + dev.address : "") + "</span></summary><pre>" +
      escapeHtml(JSON.stringify(dev, null, 2)) + "</pre></details>";
    $("#devices-list").innerHTML =
      d.devices.map(detail).join("") || '<div class="item faint">none — nothing adopted</div>';
    $("#candidates-list").innerHTML =
      (d.candidates || [])
        .map(
          (c) =>
            '<details class="item-detail"><summary><b>' + escapeHtml(c.id) + "</b> <span>" +
            escapeHtml(c.kind) + (c.address ? " @ " + c.address : "") +
            (c.adopted ? " · adopted" : "") + "</span></summary><pre>" +
            escapeHtml(JSON.stringify(c, null, 2)) + "</pre></details>"
        )
        .join("") || '<div class="item faint">no candidates</div>';
    $("#unidentified-list").innerHTML =
      (d.unidentified || [])
        .map((u) => '<div class="item"><b>' + escapeHtml(u.address) + "</b><span>" + escapeHtml(u.note || "") + "</span></div>")
        .join("") || '<div class="item faint">none</div>';
    $("#devices-mode").textContent =
      "discovery: " + (d.discovery || "auto") +
      (d.discovery === "manual"
        ? " — drivers probe but adopt nothing; use `script` or manifest_apply to take a candidate"
        : " — recognised devices are adopted automatically");
  } catch (e) {
    toast(e.message);
  }
}

async function loadTools() {
  try {
    const d = await api("/api/tools");
    $("#tools-list").innerHTML = d.tools
      .map(
        (t) =>
          '<details class="item-detail' + (t.visible ? "" : " faint") + '"><summary><b>' +
          escapeHtml(t.name) + "</b> <span>" + escapeHtml(t.category) + (t.visible ? "" : " · hidden") +
          "</span></summary><pre>" +
          escapeHtml(
            JSON.stringify(
              {
                name: t.name,
                description: t.description,
                category: t.category,
                requires: t.requires || [],
                visible: t.visible,
                mutating: t.mutating,
                mcp: t.mcp === undefined ? true : t.mcp,
                input_schema: t.input_schema || {},
              },
              null,
              2
            )
          ) +
          "</pre></details>"
      )
      .join("");
  } catch (e) {
    toast(e.message);
  }
}

async function loadAgent() {
  try {
    const d = await api("/api/agent/config");
    $("#agent-base").value = d.base_url || "";
    $("#agent-model").value = d.model || "";
    $("#agent-steps").value = d.max_steps;
  $("#agent-context").value = d.context_tokens || 8192;
  $("#agent-output").value = d.max_output_tokens || 2048;
    $("#agent-temperature").value = d.temperature;
    $("#agent-protocol").value = d.protocol || "chat";
    $("#agent-reasoning").value = d.reasoning || "";
    $("#agent-vision").value = d.vision === false ? "false" : "true";
    $("#auth-tick").value = d.tick_ms;
    const prompt = await api("/api/agent/prompt");
    kv($("#conversation-kv"), [
      ["Messages", prompt.history],
      ["Compressed lines", prompt.archive_lines],
      ["System prompt", prompt.bytes + " B"],
      ["Runs", d.run.runs],
      ["Tokens (total)", d.run.usage_total ? d.run.usage_total.total_tokens : 0],
    ]);
  } catch (e) {
    if (!String(e.message).includes("PIN")) toast(e.message);
  }
}

async function loadSched() {
  try {
    const d = await api("/api/sched");
    const tasks = d.tasks || [];
    if (!tasks.length) {
      $("#sched-list").innerHTML = '<div class="item faint">no tasks</div>';
      return;
    }
    $("#sched-list").innerHTML = tasks
      .map(
        (t) =>
          '<div class="item' + (t.enabled ? "" : " faint") + '"><b>' + escapeHtml(t.id) + "</b><span>" +
          escapeHtml(t.every_s ? "every " + t.every_s + "s" : t.at_hhmm) + " · " +
          escapeHtml(((t.script ? "script: " + t.script : t.prompt) || "").slice(0, 60)) + "</span>" +
          '<button class="btn ghost" data-sched="' + escapeHtml(t.id) + '" data-op="' +
          (t.enabled ? "sched_disable" : "sched_enable") + '">' + (t.enabled ? "Pause" : "Resume") +
          "</button>" +
          '<button class="btn ghost" data-sched="' + escapeHtml(t.id) + '" data-op="delete">Delete</button></div>'
      )
      .join("");
    $$("[data-sched]").forEach((b) => (b.onclick = () => schedOp(b.dataset.sched, b.dataset.op)));
  } catch (e) {
    toast(e.message);
  }
}

/** `sched_enable`, `sched_disable` and delete go through the same endpoint the CLI uses. */
async function schedOp(id, op) {
  try {
    await api("/api/sched", { method: "POST", body: { kind: "task", op, id } });
    loadSched();
  } catch (e) {
    toast(e.message);
  }
}

/** The workspace as one tree: directories fold, a file opens below.
 *
 * One `/api/files/tree` returns metadata, not all file contents. File-count/path budgets
 * bound the tree, and its hierarchy is the one the canonical paths already carry.
 *
 * Read-only on purpose. Writes go through the agent/file tools, where shared workspace
 * admission and incremental publication are enforced, rather than a second editor path. */
async function loadTree() {
  try {
    const d = await api("/api/files/tree");
    if (!d.ok) {
      toast(d.error || "cannot read the workspace");
      return;
    }
    const root = { dirs: new Map(), files: [] };
    for (const e of d.entries || []) {
      const parts = e.path.split("/").filter(Boolean);
      let node = root;
      for (let i = 0; i < parts.length - 1; i++) {
        const key = "/" + parts.slice(0, i + 1).join("/");
        if (!node.dirs.has(key)) node.dirs.set(key, { name: parts[i], path: key, dirs: new Map(), files: [] });
        node = node.dirs.get(key);
      }
      if (e.dir) {
        if (!node.dirs.has(e.path)) node.dirs.set(e.path, { name: e.name, path: e.path, dirs: new Map(), files: [] });
      } else {
        node.files.push(e);
      }
    }
    const box = $("#files-tree");
    box.innerHTML = "";
    renderNode(root, box, 0);
    if (!box.childElementCount) box.innerHTML = '<div class="item faint">the workspace is empty</div>';
    $("#files-total").textContent =
      d.count + " entries \u00b7 " + d.total_bytes + " of " + d.total_max +
      " bytes used \u00b7 a file may be " + d.file_max + " bytes";
  } catch (e) {
    toast(e.message);
  }
}

/** One level: directories first, then files, both alphabetical. */
function renderNode(node, box, depth) {
  const dirs = Array.from(node.dirs.values()).sort((a, b) => a.name.localeCompare(b.name));
  const files = node.files.slice().sort((a, b) => a.name.localeCompare(b.name));
  for (const dir of dirs) {
    const details = document.createElement("details");
    details.className = "tree-dir";
    // The first level is open: a tree that starts fully folded shows one word.
    details.open = depth === 0;
    const summary = document.createElement("summary");
    summary.textContent = dir.name;
    details.appendChild(summary);
    const kids = document.createElement("div");
    kids.className = "tree-kids";
    details.appendChild(kids);
    renderNode(dir, kids, depth + 1);
    box.appendChild(details);
  }
  for (const file of files) {
    const row = document.createElement("button");
    row.className = "tree-file";
    row.innerHTML = "<b>" + escapeHtml(file.name) + "</b><span class='faint'>" +
      file.size_bytes + " B</span>";
    row.onclick = () => openFile(file.path);
    box.appendChild(row);
  }
}

async function openFile(path) {
  try {
    const d = await api("/api/files/read?path=" + encodeURIComponent(path));
    if (!d.ok) {
      toast(d.error || "cannot read that file");
      return;
    }
    $("#files-viewer-name").textContent = d.path + "  (" + d.size_bytes + " B)";
    $("#files-viewer").textContent = d.text;
    $("#files-viewer-card").hidden = false;
    $("#files-viewer").scrollTop = 0;
  } catch (e) {
    toast(e.message);
  }
}

async function loadLogs() {
  try {
    const d = await api("/api/logs?n=80");
    $("#logs").textContent = (d.lines || [])
      .map((l) => (l.t_ms / 1000).toFixed(1) + "s " + l.level + " " + l.target + ": " + l.msg)
      .join("\n");
  } catch (e) {
    toast(e.message);
  }
}

/** Wait until the board reports a station address, then return the wifi object.
 *
 * "Save and join" used to print "the board is joining" and then a single status refresh four
 * seconds later, which is often before the association finishes — so the address the person
 * needs was the one thing the page never showed. Poll instead, and stop as soon as there is
 * an address (or the deadline passes, which is a wrong password, not a slow network). */
async function waitForStationIp(seconds) {
  const deadline = Date.now() + seconds * 1000;
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
    try {
      const d = await api("/api/status");
      if (d && d.wifi && d.wifi.ip) return d.wifi;
    } catch {
      /* the board may be reconfiguring the radio: try again */
    }
  }
  return null;
}

async function loadSettings() {
  try {
    const d = await api("/api/config");
    $("#wifi-ssid").value = d.wifi.ssid || "";
    $("#wifi-gateway").value = d.wifi.gateway || "";
    $("#wifi-dns").value = d.wifi.dns || "";
    $("#wifi-password").value = "";
    // Where the board is right now, not only where it is trying to go.
    const s = await api("/api/status");
    if (s && s.wifi && s.wifi.ip) {
      const ap = s.wifi.state === "ap+sta" ? " \u00b7 setup AP still up" : "";
      $("#wifi-result").textContent = "currently at http://" + s.wifi.ip + "/" + ap;
    }
  } catch (e) {
    if (!String(e.message).includes("PIN")) toast(e.message);
  }
}

/** Run one panel read with the loader around it.
 *
 * The loader counts reads rather than panels, so entering Agent (model + schedule) keeps it
 * up until both answers are in, and a second tab opened mid-flight cannot hide the first
 * one's spinner. */
function panelLoad(label, read) {
  beginPanelLoad(label);
  return read().finally(endPanelLoad);
}

function showPanel(name) {
  S.panel = name;
  $$(".tabs button").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.panel === name)));
  $$(".panel").forEach((p) => (p.hidden = p.id !== "panel-" + name));
  if (name === "devices") panelLoad("Loading devices\u2026", loadDevices);
  if (name === "tools") panelLoad("Loading tools\u2026", loadTools);
  // The schedule queues turns for this agent, so it lives on the Agent panel; the log is
  // the other thing to read when the board misbehaves, so it lives on Settings.
  if (name === "agent") {
    panelLoad("Loading the agent\u2026", loadAgent);
    panelLoad("Loading the schedule\u2026", loadSched);
  }
  if (name === "files") panelLoad("Loading files\u2026", loadTree);
  if (name === "settings") {
    panelLoad("Loading settings\u2026", loadSettings);
    panelLoad("Loading the log\u2026", loadLogs);
  }
}

// ---------------------------------------------------------------- wiring

function autosize() {
  const el = $("#input");
  el.style.height = "auto";
  el.style.height = Math.min(el.scrollHeight, 132) + "px";
  fitChatToComposer();
}

/** How far the composer was lifted off the bottom, by the keyboard listener below.
 *
 * Zero now: the composer is a flex child of a body whose height is the visual viewport, so
 * there is no bar to lift. Kept because the transcript's padding and the page-layout test
 * both read it. */
let composerLift = 0;

/** Reserve exactly what the composer covers at the bottom of the transcript.
 *
 * The composer is in the flex column now, so the transcript does not have to reserve room
 * for it: the two are laid out one after the other. A small gap is all that is left. */
function fitChatToComposer() {
  const composer = $("#composer");
  const chat = $("#chat");
  if (!composer || !chat) return 0;
  const pad = 12;
  chat.style.paddingBottom = pad + "px";
  return pad;
}

/** Keep the page as tall as the part of it the keyboard leaves visible.
 *
 * A phone keyboard shrinks `visualViewport` without necessarily shrinking the layout
 * viewport. Binding the document height (`--vvh`, which `body` and the sheet use) to the
 * visual viewport is what keeps the composer's bottom edge above the keys; `position:
 * fixed` could not do that, which is why the field was half covered. */
function trackViewport() {
  const vv = window.visualViewport;
  const apply = () => {
    const h = vv ? vv.height : window.innerHeight;
    document.documentElement.style.setProperty("--vvh", h + "px");
    composerLift = 0;
    fitChatToComposer();
    if (document.activeElement === $("#input")) scrollToEnd();
  };
  if (vv) {
    vv.addEventListener("resize", apply);
    vv.addEventListener("scroll", apply);
  }
  window.addEventListener("resize", apply);
  window.addEventListener("orientationchange", () => setTimeout(apply, 250));
  // Android reports the new visual viewport a little after focus, and the keyboard animation
  // settles later still: reapply once after the keyboard has had time to appear.
  $("#input").addEventListener("focus", () => setTimeout(apply, 350));
  $("#input").addEventListener("blur", () => setTimeout(apply, 250));
  if (window.ResizeObserver) {
    new ResizeObserver(() => fitChatToComposer()).observe($("#composer"));
  }
  apply();
}

function init() {
  $("#menu-btn").onclick = () => {
    $("#sheet").hidden = false;
    // Always open on Devices: it is the page a person needs most often (what is this board,
    // is anything missing), and remembering the last tab meant landing on Settings after
    // one visit.
    showPanel("devices");
  };
  $("#sheet-close").onclick = () => ($("#sheet").hidden = true);
  $$(".tabs button").forEach((b) => (b.onclick = () => showPanel(b.dataset.panel)));

  $("#composer").onsubmit = (e) => {
    e.preventDefault();
    send($("#input").value);
  };

  $("#clear-chat").onclick = () => changeConversation("/api/agent/reset", {}, true);

  // A schedule or another client can start a run without this page's stream. Redraw only
  // when the transcript changes, and never let an old poll race a clear or a local send.
  setInterval(pollHistory, 20000);

  // One image per message, downscaled to fit the request and the board's heap.
  $("#attach").onclick = () => $("#attach-input").click();
  $("#attach-input").onchange = async (ev) => {
    const file = ev.target.files && ev.target.files[0];
    ev.target.value = "";
    if (!file) return;
    if (!file.type.startsWith("image/")) {
      toast("only images can be attached");
      return;
    }
    try {
      S.attach = await downscale(file);
      showAttach();
    } catch (e) {
      toast(e.message);
    }
  };
  $("#input").addEventListener("input", autosize);
  $("#input").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send($("#input").value);
    }
  });

  $("#login-go").onclick = doLogin;
  $("#login-pin").addEventListener("keydown", (e) => {
    if (e.key === "Enter") doLogin();
  });

  $("#devices-probe").onclick = async () => {
    beginPanelLoad("Probing the buses\u2026");
    try {
      await api("/api/devices/probe", { method: "POST", body: {} });
      toast("probing");
      setTimeout(() => panelLoad("Loading devices\u2026", loadDevices), 1500);
    } catch (e) {
      toast(e.message);
    } finally {
      endPanelLoad();
    }
  };
  $("#devices-refresh").onclick = () => panelLoad("Loading devices\u2026", loadDevices);

  $("#agent-save").onclick = async () => {
    const body = {
      base_url: $("#agent-base").value.trim(),
      model: $("#agent-model").value.trim(),
      max_steps: Number($("#agent-steps").value || 0),
      context_tokens: Number($("#agent-context").value || 8192),
      max_output_tokens: Number($("#agent-output").value || 2048),
      temperature: Number($("#agent-temperature").value || 0.2),
      protocol: $("#agent-protocol").value,
      reasoning: $("#agent-reasoning").value,
      vision: $("#agent-vision").value === "true",
    };
    const key = $("#agent-key").value.trim();
    if (key) body.api_key = key;
    try { await api("/api/agent/config", { method: "POST", body }); $("#agent-key").value = ""; toast("applied; save queued"); loadStatus(); }
    catch (e) { toast(e.message); }
  };

  $("#forget-tail").onclick = () => changeConversation("/api/agent/forget", { keep: 1 }, false);
  $("#forget-all").onclick = () => changeConversation("/api/agent/forget", {}, true);

  $("#sched-add").onclick = async () => {
    try {
      await api("/api/sched", {
        method: "POST",
        body: {
          kind: "task",
          op: "add",
          task: {
            prompt: $("#sched-prompt").value.trim(),
            script: $("#sched-script").value.trim(),
            every_s: Number($("#sched-every").value || 0),
            at_hhmm: $("#sched-at").value.trim(),
          },
        },
      });
      $("#sched-prompt").value = "";
      $("#sched-script").value = "";
      loadSched();
      toast("task added");
    } catch (e) { toast(e.message); }
  };

  $("#wifi-connect").onclick = async () => {
    $("#wifi-result").textContent = "joining…";
    try {
      // The gateway and DNS belong to the device config, not to the join request: they
      // describe the network the board sits on, not the credentials it uses.
      await api("/api/config", {
        method: "POST",
        body: { wifi: { gateway: $("#wifi-gateway").value.trim(), dns: $("#wifi-dns").value.trim() } },
      });
      await api("/api/wifi/connect", {
        method: "POST",
        body: { ssid: $("#wifi-ssid").value.trim(), password: $("#wifi-password").value },
      });
      $("#wifi-result").textContent = "credentials queued; the board is joining";
      // The address arrives a second or two after the association, and it is the one thing a
      // person needs from this screen: the portal they are reading it on is about to answer at
      // a different address, and the setup AP is closed as soon as the station has one.
      const joined = await waitForStationIp(25);
      if (joined) {
        const closed = joined.state === "connected" ? " \u00b7 setup AP closed" : "";
        $("#wifi-result").textContent = "joined \u2014 board is at http://" + joined.ip + "/" + closed;
        toast("board is at " + joined.ip);
        loadStatus();
      } else {
        $("#wifi-result").textContent =
          "no address yet \u2014 check the password, or reconnect to esp32-agent-setup";
      }
    } catch (e) { $("#wifi-result").textContent = e.message; }
  };
  $("#wifi-scan").onclick = async () => {
    $("#wifi-result").textContent = "scanning…";
    try {
      const d = await api("/api/wifi/scan");
      $("#wifi-result").textContent = (d.networks || []).map((n) => n.ssid + " (" + n.rssi + ")").join(", ") || "none";
    } catch (e) { $("#wifi-result").textContent = e.message; }
  };

  $("#settings-save").onclick = async () => {
    const body = { agent: { tick_ms: Number($("#auth-tick").value || 1000) } };
    const pin = $("#auth-pin").value.trim();
    if (pin) body.auth = { portal_pin: pin };
    try {
      await api("/api/config", { method: "POST", body });
      $("#auth-pin").value = "";
      // A new PIN invalidates the token the page is holding.
      if (pin) { S.token = ""; localStorage.removeItem("esp32-agent.token"); }
      toast("applied; save queued");
    } catch (e) { toast(e.message); }
  };

  $("#reboot").onclick = async () => {
    if (!(await askConfirm(
      "Restart the board?",
      "The agent, its tools and any open stream stop answering until it boots again.",
      "Restart",
    ))) return;
    try { await api("/api/reboot", { method: "POST", body: {} }); toast("restarting"); }
    catch (e) { toast(e.message); }
  };

  $("#logs-refresh").onclick = loadLogs;
  $("#files-back").onclick = () => {
    $("#files-viewer-card").hidden = true;
  };

  loadHistory();
  loadStatus();
  trackViewport();
  wirePullToRefresh();
  // Not connected at load: the stream is opened when a run starts and closed when it ends.
  setInterval(() => {
    if (!$("#sheet").hidden) return;
    loadStatus();
  }, 5000);
}

document.addEventListener("DOMContentLoaded", init);
