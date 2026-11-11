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

/** Words the tests and the UI share: one place, so a label cannot go missing. */
const LABELS = {
  goal_objective: "Objective",
  goal_notes: "Notes",
  goal_started: "Started",
  goal_status_stopped: "Stopped",
};

const S = {
  token: localStorage.getItem("esp32-agent.token") || "",
  lastSeq: 0,
  stream: null,
  busy: false,
  panel: "status",
  afterUnlock: null,
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

/** A small Markdown subset: the four things a model actually emits on a 2 KB budget. */
function markdown(text) {
  const blocks = [];
  let src = escapeHtml(text || "").replace(/```([\s\S]*?)```/g, (_, code) => {
    blocks.push(code.replace(/^\w*\n/, ""));
    return "\u0000BLOCK" + (blocks.length - 1) + "\u0000";
  });

  src = src
    .replace(/^### (.*)$/gm, "<h3>$1</h3>")
    .replace(/^## (.*)$/gm, "<h2>$1</h2>")
    .replace(/^# (.*)$/gm, "<h1>$1</h1>")
    .replace(/^&gt; (.*)$/gm, "<blockquote>$1</blockquote>")
    .replace(/^[-*] (.*)$/gm, "<li>$1</li>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');

  // Wrap runs of list items, then paragraphs.
  src = src.replace(/(?:<li>.*<\/li>\n?)+/g, (m) => "<ul>" + m.replace(/\n/g, "") + "</ul>");
  const html = src
    .split(/\n{2,}/)
    .map((chunk) => {
      const t = chunk.trim();
      if (!t) return "";
      if (/^<(h\d|ul|blockquote|pre|p|\u0000BLOCK)/.test(t)) return t;
      return "<p>" + t.replace(/\n/g, "<br>") + "</p>";
    })
    .join("");
  return html.replace(/\u0000BLOCK(\d+)\u0000/g, (_, i) => "<pre><code>" + blocks[Number(i)] + "</code></pre>");
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

function addTools(row) {
  let tools = row.querySelector(".tools");
  if (!tools) {
    tools = document.createElement("div");
    tools.className = "tools";
    row.appendChild(tools);
  }
  return tools;
}

function chip(tools, label) {
  const el = document.createElement("span");
  el.className = "chip run";
  el.textContent = label;
  tools.appendChild(el);
  return el;
}

function scrollToEnd() {
  const chat = $("#chat");
  chat.scrollTop = chat.scrollHeight;
}

function renderMessage(msg) {
  if (msg.role === "tool") return;
  const who = msg.role === "user" ? "user" : "agent";
  const text = msg.text || "";
  if (!text && !(msg.tool_calls && msg.tool_calls.length)) return;
  const { row, bubble } = addRow(who);
  bubble.innerHTML = markdown(text);
  if (msg.tool_calls && msg.tool_calls.length) {
    const tools = addTools(row);
    for (const call of msg.tool_calls) {
      const name = call.function ? call.function.name : call.name;
      chip(tools, name);
    }
  }
  if (who === "agent") {
    const meta = document.createElement("div");
    meta.className = "meta";
    meta.textContent = new Date(msg.at_ms || Date.now()).toLocaleTimeString();
    row.appendChild(meta);
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
    for (const m of msgs) {
      renderMessage(m);
      S.lastSeq = Math.max(S.lastSeq, m.seq || 0);
    }
  } catch (e) {
    if (String(e.message).includes("PIN")) return;
    toast("history: " + e.message);
  }
}

async function send(text) {
  const trimmed = text.trim();
  if (!trimmed) return;
  const { bubble } = addRow("user");
  bubble.innerHTML = markdown(trimmed);
  $("#input").value = "";
  autosize();
  try {
    await api("/api/agent/message", { method: "POST", body: { text: trimmed } });
    S.busy = true;
    $("#link").className = "dot busy";
    pendingRow = null;
  } catch (e) {
    toast(e.message);
  }
}

// ---------------------------------------------------------------- streaming

let pendingRow = null;

function ensurePending() {
  if (pendingRow) return pendingRow;
  const { row, bubble } = addRow("agent");
  bubble.innerHTML = '<span class="typing"><i></i><i></i><i></i></span>';
  pendingRow = row;
  return row;
}

function handleEvent(name, data) {
  S.lastSeq = Math.max(S.lastSeq, data.seq || 0);
  switch (name) {
    case "message_start":
      S.busy = true;
      $("#link").className = "dot busy";
      break;
    case "text_delta": {
      const row = ensurePending();
      const bubble = row.querySelector(".bubble");
      if (bubble.querySelector(".typing")) bubble.innerHTML = "";
      bubble.dataset.raw = (bubble.dataset.raw || "") + (data.text || "");
      bubble.innerHTML = markdown(bubble.dataset.raw);
      scrollToEnd();
      break;
    }
    case "tool_use": {
      const row = pendingRow || ensurePending();
      chip(addTools(row), (data.name || "tool") + "()");
      break;
    }
    case "tool_result": {
      const row = pendingRow;
      if (!row) break;
      const chips = Array.from(row.querySelectorAll(".chip.run"));
      const last = chips[chips.length - 1];
      if (last) last.className = "chip " + (data.is_error ? "err" : "ok");
      break;
    }
    case "error":
      toast(data.text || "run failed");
      break;
    case "run_end":
      S.busy = false;
      if (pendingRow) {
        const bubble = pendingRow.querySelector(".bubble");
        if (bubble.querySelector(".typing")) bubble.innerHTML = "<p><em>no answer</em></p>";
      }
      pendingRow = null;
      $("#link").className = "dot live";
      break;
  }
}

const EVENT_KINDS = [
  "message_start", "thinking_start", "thinking_delta", "thinking_end",
  "text_start", "text_delta", "text_end", "tool_use", "tool_progress",
  "tool_result", "usage", "message_end", "error", "run_end",
];

function connectStream() {
  if (S.stream) S.stream.close();
  const es = new EventSource("/api/agent/stream?after=" + S.lastSeq);
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
    setTimeout(connectStream, 1500);
  };
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
    ]);
    kv($("#wifi-kv"), [
      ["State", d.wifi.state],
      ["SSID", d.wifi.ssid || "-"],
      ["IP", d.wifi.ip || "-"],
      ["AP", d.wifi.ap_ip],
    ]);
    const model = await api("/api/agent/config");
    kv($("#model-kv"), [
      ["Model", model.model || "(none)"],
      ["Endpoint", model.base_url || "(none)"],
      ["Key", model.api_key_set ? "set" : "not set"],
      ["Max steps", model.max_steps === 0 ? "unlimited" : model.max_steps],
      ["Runs", model.run.runs],
      ["Last error", (model.run.last_error || "-").slice(0, 60)],
    ]);
    $("#sub").textContent =
      (d.wifi.ip || d.wifi.ap_ip) + " · " + d.devices.count + " devices · " + d.mcp.tools_visible + " tools · " +
      (model.model || "no model");
    $("#title").textContent = d.device_name || "esp32-agent";
  } catch (e) {
    if (!String(e.message).includes("PIN")) $("#sub").textContent = "offline";
  }
}

async function loadDevices() {
  try {
    const d = await api("/api/devices");
    $("#devices-list").innerHTML = d.devices
      .map(
        (dev) =>
          '<div class="item"><b>' + escapeHtml(dev.id) + "</b><span>" + escapeHtml(dev.name) +
          (dev.address ? " @ " + dev.address : "") + "</span></div>"
      )
      .join("") || '<div class="item faint">none</div>';
    $("#unidentified-list").innerHTML =
      (d.unidentified || [])
        .map((u) => '<div class="item"><b>' + escapeHtml(u.address) + "</b><span>" + escapeHtml(u.note || "") + "</span></div>")
        .join("") || '<div class="item faint">none</div>';
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
          '<div class="item' + (t.visible ? "" : " faint") + '"><b>' + escapeHtml(t.name) +
          "</b><span>" + escapeHtml(t.category) + "</span></div>"
      )
      .join("");
  } catch (e) {
    toast(e.message);
  }
}

async function loadManifest() {
  try {
    const d = await api("/api/manifest");
    $("#manifest-text").value = JSON.stringify(d.manifest || {}, null, 2);
  } catch (e) {
    toast(e.message);
  }
}

async function applyManifest(dryRun) {
  const out = $("#manifest-result");
  out.textContent = dryRun ? "validating…" : "applying…";
  try {
    const manifest = JSON.parse($("#manifest-text").value);
    const d = await api("/api/manifest", { method: "POST", body: { manifest, dry_run: dryRun } });
    const issues = d.issues || [];
    out.textContent = (d.valid === false ? "invalid: " : "ok: ") + (issues.length ? JSON.stringify(issues).slice(0, 400) : "no issues");
    if (!dryRun) {
      loadTools();
      loadStatus();
    }
  } catch (e) {
    out.textContent = e.message;
  }
}

async function loadAgent() {
  try {
    const d = await api("/api/agent/config");
    $("#agent-base").value = d.base_url || "";
    $("#agent-model").value = d.model || "";
    $("#agent-steps").value = d.max_steps;
    $("#agent-temperature").value = d.temperature;
    $("#auth-tick").value = d.tick_ms;
    const goal = await api("/api/goal");
    $("#goal-objective").value = goal.objective || "";
    $("#goal-max-iterations").value = goal.max_iterations || 10;
    $("#goal-notes").textContent =
      (goal.status || "idle") + "\n" + (goal.notes || []).join("\n");
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
          escapeHtml((t.prompt || "").slice(0, 60)) + "</span>" +
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
    $("#wifi-password").value = "";
  } catch (e) {
    if (!String(e.message).includes("PIN")) toast(e.message);
  }
}

function showPanel(name) {
  S.panel = name;
  $$(".tabs button").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.panel === name)));
  $$(".panel").forEach((p) => (p.hidden = p.id !== "panel-" + name));
  if (name === "status") loadStatus();
  if (name === "devices") loadDevices();
  if (name === "tools") loadTools();
  if (name === "manifest") loadManifest();
  if (name === "agent") loadAgent();
  if (name === "schedule") loadSched();
  if (name === "logs") loadLogs();
  if (name === "settings") loadSettings();
}

// ---------------------------------------------------------------- wiring

function autosize() {
  const el = $("#input");
  el.style.height = "auto";
  el.style.height = Math.min(el.scrollHeight, 132) + "px";
}

function init() {
  $("#menu-btn").onclick = () => {
    $("#sheet").hidden = false;
    showPanel(S.panel);
  };
  $("#sheet-close").onclick = () => ($("#sheet").hidden = true);
  $$(".tabs button").forEach((b) => (b.onclick = () => showPanel(b.dataset.panel)));

  $("#composer").onsubmit = (e) => {
    e.preventDefault();
    send($("#input").value);
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
    try { await api("/api/devices/probe", { method: "POST", body: {} }); toast("probing"); setTimeout(loadDevices, 1200); }
    catch (e) { toast(e.message); }
  };
  $("#manifest-validate").onclick = () => applyManifest(true);
  $("#manifest-apply").onclick = () => applyManifest(false);
  $("#manifest-reload").onclick = loadManifest;

  $("#agent-save").onclick = async () => {
    const body = {
      base_url: $("#agent-base").value.trim(),
      model: $("#agent-model").value.trim(),
      max_steps: Number($("#agent-steps").value || 0),
      temperature: Number($("#agent-temperature").value || 0.2),
    };
    const key = $("#agent-key").value.trim();
    if (key) body.api_key = key;
    try { await api("/api/agent/config", { method: "POST", body }); $("#agent-key").value = ""; toast("saved"); loadStatus(); }
    catch (e) { toast(e.message); }
  };

  $("#goal-start").onclick = async () => {
    try {
      await api("/api/goal", {
        method: "POST",
        body: { objective: $("#goal-objective").value.trim(), max_iterations: Number($("#goal-max-iterations").value || 10) },
      });
      toast(LABELS.goal_started);
      loadAgent();
    } catch (e) { toast(e.message); }
  };
  $("#goal-stop").onclick = async () => {
    try { await api("/api/goal/stop", { method: "POST", body: {} }); toast(LABELS.goal_status_stopped); loadAgent(); }
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
            every_s: Number($("#sched-every").value || 0),
            at_hhmm: $("#sched-at").value.trim(),
          },
        },
      });
      $("#sched-prompt").value = "";
      loadSched();
      toast("task added");
    } catch (e) { toast(e.message); }
  };

  $("#wifi-connect").onclick = async () => {
    $("#wifi-result").textContent = "joining…";
    try {
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

  loadHistory();
  loadStatus();
  connectStream();
  setInterval(() => { if (!$("#sheet").hidden) return; loadStatus(); }, 5000);
}

document.addEventListener("DOMContentLoaded", init);
