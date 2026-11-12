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
  lastEvent: 0,
  rendered: 0,
  stream: null,
  busy: false,
  panel: "status",
  afterUnlock: null,
  filesPath: "/",
};

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
  if (!res.ok) throw new Error((data && data.error) || res.status + " " + res.statusText);
  return data;
}

function unlock() {
  return new Promise((resolve) => {
    S.afterUnlock = resolve;
    $("#login").hidden = false;
    setTimeout(() => $("#login-pin").focus(), 60);
  });
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

/** Inline spans, applied to text that has already been HTML-escaped. */
function inline(text) {
  return text
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
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

/** A small Markdown subset: what a model actually emits on a 2 KB budget. */
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

async function loadHistory() {
  try {
    const data = await api("/api/agent/history");
    const chat = $("#chat");
    chat.innerHTML = "";
    const msgs = (data && data.messages) || [];
    if (!msgs.length) {
      const { bubble } = addRow("agent");
      bubble.innerHTML =
        "<p>This is the board's own agent. Ask it about the hardware, tell it to change something, or give it a job &mdash; it calls its tools by itself.</p>";
      return;
    }
    for (const turn of groupTurns(msgs)) renderTurn(turn);
    S.rendered = msgs.length;
    for (const m of msgs) S.lastMsg = Math.max(S.lastMsg, m.seq || 0);
    scrollToEnd();
  } catch (e) {
    if (String(e.message).includes("PIN")) return;
    toast("history: " + e.message);
  }
}

/** Downscale an image in the browser, before it crosses the LAN.
 *
 * The board keeps an attachment in RAM and a request has to fit what its TLS stack can
 * send, so a 4 MB phone photo is not a thing to upload: 1024 px on the long side as JPEG
 * is ~20 kB and still legible. If it is still too big after that, say so instead of
 * handing the board a request it will refuse. */
function downscale(file) {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      const max = 1024;
      const scale = Math.min(1, max / Math.max(img.width, img.height));
      const w = Math.max(1, Math.round(img.width * scale));
      const h = Math.max(1, Math.round(img.height * scale));
      const canvas = document.createElement("canvas");
      canvas.width = w;
      canvas.height = h;
      canvas.getContext("2d").drawImage(img, 0, 0, w, h);
      let dataUrl = canvas.toDataURL("image/jpeg", 0.72);
      // Step the quality down once if the first pass is over the cap.
      if (dataUrl.length * 0.75 > 24000) dataUrl = canvas.toDataURL("image/jpeg", 0.5);
      const data_b64 = dataUrl.split(",", 2)[1] || "";
      resolve({ name: file.name || "image.jpg", mime: "image/jpeg", data_b64, kb: Math.round((data_b64.length * 0.75) / 1024) });
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
  if (!trimmed) return;
  const { bubble } = addRow("user");
  bubble.innerHTML = markdown(trimmed + (S.attach ? "\n\n_[image: " + S.attach.name + "]_" : ""));
  $("#input").value = "";
  autosize();
  const attachment = S.attach;
  S.attach = null;
  showAttach();
  try {
    const payload = { text: trimmed };
    if (attachment) payload.attachments = [attachment];
    await api("/api/agent/message", { method: "POST", body: payload });
    S.busy = true;
    $("#link").className = "dot busy";
    liveTrace = null;
    liveBubble = null;
    liveCalls = 0;
    connectStream();
  } catch (e) {
    toast(e.message);
  }
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
  S.lastEvent = Math.max(S.lastEvent, data.seq || 0);
  switch (name) {
    case "message_start":
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
      setTimeout(stopStream, 500);
      break;
  }
}

const EVENT_KINDS = [
  "message_start", "thinking_start", "thinking_delta", "thinking_end",
  "text_start", "text_delta", "text_end", "tool_use", "tool_progress",
  "tool_result", "retried", "usage", "message_end", "error", "run_end",
];

function connectStream() {
  if (S.stream) return;
  const es = new EventSource("/api/agent/stream?after=" + S.lastEvent);
  S.stream = es;
  for (const kind of EVENT_KINDS) {
    es.addEventListener(kind, (ev) => {
      let data = {};
      try { data = JSON.parse(ev.data); } catch { return; }
      handleEvent(kind, data);
    });
  }
  es.onopen = () => { $("#link").className = S.busy ? "dot busy" : "dot live"; };
  // The board closes the stream after 30 s of silence or 120 s total; EventSource would
  // resume with Last-Event-ID, which this server does not read, so reconnect by hand.
  es.onerror = () => {
    es.close();
    S.stream = null;
    $("#link").className = "dot off";
    if (S.busy) setTimeout(connectStream, 1500);
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

function kv(el, pairs) {
  el.innerHTML = pairs
    .map(([k, v]) => "<span>" + escapeHtml(k) + "</span><span>" + escapeHtml(String(v)) + "</span>")
    .join("");
}

async function loadStatus() {
  try {
    const d = await api("/api/status");
    kv($("#status-kv"), [
      ["Name", d.device_name],
      ["Profile", d.board.profile],
      ["Chip", d.board.soc + " / " + d.board.cores + " cores"],
      ["Uptime", Math.floor(d.uptime_s / 60) + " min"],
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

/** The workspace, one directory at a time: a file list you can tap into, and a file view.
 *
 * Read-only on purpose. Every write goes through the agent or the `write` tool, where the
 * workspace's 8 KB budget and its one-blob-in-NVS storage are enforced; a portal editor that
 * writes would need the same checks a second time and would be the place they drift apart. */
async function loadFiles(path) {
  S.filesPath = path || "/";
  try {
    const d = await api("/api/files?path=" + encodeURIComponent(S.filesPath));
    if (!d.ok) {
      toast(d.error || "cannot list that directory");
      return;
    }
    S.filesPath = d.path || "/";
    renderCrumbs(S.filesPath);
    const rows = (d.entries || []).map((e) => {
      const size = e.dir ? "dir" : e.size_bytes + " B";
      return (
        '<button class="item file-row" data-path="' + escapeHtml(e.path) + '" data-dir="' +
        (e.dir ? "1" : "0") + '"><b>' + escapeHtml(e.name) + (e.dir ? "/" : "") +
        "</b><span class='faint'>" + escapeHtml(size) + "</span></button>"
      );
    });
    $("#files-list").innerHTML = rows.join("") || '<div class="item faint">empty</div>';
    $("#files-total").textContent =
      (d.count || 0) + " entries \u00b7 " + d.total_bytes + " of " + d.total_max +
      " bytes used \u00b7 a file may be " + d.file_max + " bytes";
    $("#files-viewer-card").hidden = true;
    $$("#files-list .file-row").forEach((row) => {
      row.onclick = () => {
        if (row.dataset.dir === "1") loadFiles(row.dataset.path);
        else openFile(row.dataset.path);
      };
    });
  } catch (e) {
    toast(e.message);
  }
}

/** `/memory/pinout.md` as clickable parents, so a deep path is one tap from the root. */
function renderCrumbs(path) {
  const parts = String(path).split("/").filter(Boolean);
  let built = "";
  const crumbs = ['<a href="#" data-path="/">/</a>'];
  for (const part of parts) {
    built += "/" + part;
    crumbs.push('<a href="#" data-path="' + escapeHtml(built) + '">' + escapeHtml(part) + "</a>");
  }
  $("#files-path").innerHTML = crumbs.join(" / ");
  $$("#files-path a").forEach((a) => {
    a.onclick = (ev) => {
      ev.preventDefault();
      loadFiles(a.dataset.path);
    };
  });
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

async function loadSettings() {
  try {
    const d = await api("/api/config");
    $("#wifi-ssid").value = d.wifi.ssid || "";
    $("#wifi-gateway").value = d.wifi.gateway || "";
    $("#wifi-dns").value = d.wifi.dns || "";
    $("#wifi-password").value = "";
  } catch (e) {
    if (!String(e.message).includes("PIN")) toast(e.message);
  }
}

function showPanel(name) {
  S.panel = name;
  $$(".tabs button").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.panel === name)));
  $$(".panel").forEach((p) => (p.hidden = p.id !== "panel-" + name));
  if (name === "devices") loadDevices();
  if (name === "tools") loadTools();
  // The schedule queues turns for this agent, so it lives on the Agent panel; the log is
  // the other thing to read when the board misbehaves, so it lives on Settings.
  if (name === "agent") { loadAgent(); loadSched(); }
  if (name === "files") loadFiles(S.filesPath || "/");
  if (name === "settings") { loadSettings(); loadLogs(); }
}

// ---------------------------------------------------------------- wiring

function autosize() {
  const el = $("#input");
  el.style.height = "auto";
  el.style.height = Math.min(el.scrollHeight, 132) + "px";
  fitChatToComposer();
}

/** How far the composer was lifted off the bottom, by the keyboard listener below. */
let composerLift = 0;

/** Reserve exactly what the composer covers at the bottom of the transcript.
 *
 * The chat's own `padding-bottom` is what keeps the last message from disappearing behind
 * the composer, and it used to be a flat 84 px — right only for a one-line field at `bottom:
 * 0`. A field that has grown to two or three rows, or a composer lifted above the keyboard
 * by `composerLift`, is taller than that, and the last bubble is then cut off by the bar.
 * Measuring the composer costs a `getBoundingClientRect` and cannot drift from it. */
function fitChatToComposer() {
  const composer = $("#composer");
  const chat = $("#chat");
  if (!composer || !chat) return 0;
  const pad = Math.ceil(composer.getBoundingClientRect().height + composerLift + 10);
  chat.style.paddingBottom = pad + "px";
  return pad;
}

/** Keep the composer above the on-screen keyboard.
 *
 * A phone keyboard shrinks `visualViewport` without shrinking the layout viewport, so a
 * fixed composer stays where it was and the last line of the field ends up under the keys.
 * Binding the document height to the visual viewport (and scrolling the field into view on
 * focus) puts it back on top of the keyboard. */
function trackViewport() {
  const vv = window.visualViewport;
  const apply = () => {
    const h = vv ? vv.height : window.innerHeight;
    document.documentElement.style.setProperty("--vvh", h + "px");
    const composer = $("#composer");
    if (vv && composer) {
      const gap = Math.max(0, window.innerHeight - (vv.height + vv.offsetTop));
      composerLift = gap;
      composer.style.transform = `translateY(-${gap}px)`;
    }
    fitChatToComposer();
    if (document.activeElement === $("#input")) scrollToEnd();
  };
  if (vv) {
    vv.addEventListener("resize", apply);
    vv.addEventListener("scroll", apply);
  }
  window.addEventListener("orientationchange", () => setTimeout(apply, 250));
  // The field grows on its own (up to 132 px) while it is typed into; the transcript's
  // padding has to follow that, not a number chosen for an empty composer.
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

  // One image per message. It is downscaled in the browser, because what crosses the LAN
  // has to fit the request the board can send and the heap it can spare.
  // A run can start without this page — a schedule, another client — and the event
// stream only follows the run this page asked for. So look every 20 s, and only redraw
// when the transcript actually changed: a phone should not spend its battery on a page
// that has nothing new to say.
setInterval(async () => {
  if (S.busy || S.stream) return;
  try {
    const data = await api("/api/agent/history");
    const count = ((data && data.messages) || []).length;
    if (count !== S.rendered) loadHistory();
  } catch {
    /* offline or locked: the next tick tries again */
  }
}, 20000);

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
    try { await api("/api/devices/probe", { method: "POST", body: {} }); toast("probing"); setTimeout(loadDevices, 1500); }
    catch (e) { toast(e.message); }
  };
  $("#devices-refresh").onclick = loadDevices;

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
    try { await api("/api/agent/config", { method: "POST", body }); $("#agent-key").value = ""; toast("saved"); loadStatus(); }
    catch (e) { toast(e.message); }
  };

  $("#forget-tail").onclick = async () => {
    try { const r = await api("/api/agent/forget", { method: "POST", body: { keep: 1 } }); toast("forgot " + r.removed); loadHistory(); }
    catch (e) { toast(e.message); }
  };
  $("#forget-all").onclick = async () => {
    try { const r = await api("/api/agent/forget", { method: "POST", body: {} }); toast("forgot " + r.removed); loadHistory(); }
    catch (e) { toast(e.message); }
  };

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
      $("#wifi-result").textContent = "saved; the board is joining";
      setTimeout(loadStatus, 4000);
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
      toast("saved");
    } catch (e) { toast(e.message); }
  };

  $("#reboot").onclick = async () => {
    if (!confirm("Reboot the board?")) return;
    try { await api("/api/reboot", { method: "POST", body: {} }); toast("rebooting"); }
    catch (e) { toast(e.message); }
  };

  $("#logs-refresh").onclick = loadLogs;
  $("#files-back").onclick = () => {
    $("#files-viewer-card").hidden = true;
    loadFiles(S.filesPath);
  };

  loadHistory();
  loadStatus();
  trackViewport();
  // Not connected at load: the stream is opened when a run starts and closed when it ends.
  setInterval(() => {
    if (!$("#sheet").hidden) return;
    loadStatus();
  }, 5000);
}

document.addEventListener("DOMContentLoaded", init);
