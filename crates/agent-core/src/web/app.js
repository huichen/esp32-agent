// esp32-agent portal. No build step, no framework: the board serves this file.
// English only, deliberately: the markup carries the static labels, and this table
// holds only the text the script builds at runtime, so no string lives in two places.
"use strict";

const TEXT = {
  // Status words and card headers that renderX() fills in.
  agent_key_set: "set, leave empty to keep it", agent_key_none: "no key (no Authorization header)",
  agent_queued: "Queued", agent_running: "running", agent_idle: "idle",
  agent_empty: "No conversation yet. Send a message and the model works with the board's own tools.",
  stream_live: "live", stream_connecting: "connecting", stream_offline: "offline", stream_paused: "paused",
  stream_pause: "Pause", stream_resume: "Resume",
  stream_empty: "No events yet. While this tab is open, every step of a run shows up here as it happens.",
  stream_skipped: "{n} skipped",
  goal_objective: "Objective", goal_iter: "iteration {i} of {m}",
  goal_failures: "consecutive failures", goal_last: "Last round", goal_reason: "Stop reason",
  goal_notes: "Notes (newest last)", goal_no_notes: "No notes yet — the first iteration writes one.",
  goal_idle: "No goal yet. Describe what it should achieve, then press Start.",
  goal_need_objective: "Write an objective first", goal_started: "Started",
  // `goal_status_` is prefixed onto whatever the board reports.
  goal_status_idle: "idle", goal_status_running: "running", goal_status_met: "met",
  goal_status_blocked: "blocked", goal_status_exhausted: "exhausted", goal_status_stopped: "stopped",
  sched_enable: "Enable", sched_disable: "Disable", del: "Delete", none: "(none)",
  // Table cells the script writes.
  col_id: "id", col_device: "device", col_bus: "bus", col_caps: "caps", col_tool: "tool", col_tools: "tools",
  col_state: "state", col_detection: "detected by", col_category: "category", col_requires: "requires",
  col_description: "description", col_visible: "visible",
  no_devices: "No devices detected. Wire one up and press Re-probe.",
  no_networks: "No networks found.",
  // Toasts and inline validation, shown instead of the board's own error when it has none.
  saved: "Saved", applied: "Applied", failed: "Failed",
  need_prompt: "prompt is required", need_tool: "tool is required", need_ssid: "SSID is required",
  default_rule_prompt: "Check the board's current state.",
  bad_json: "JSON parse failed: ",
};

const state = {
  pin: localStorage.getItem("pin") || "",
  status: null,
  config: null,
  devices: [],
  tools: [],
  manifest: { revision: 0, manifest: { version: 1, devices: [], tools: [] } },
  agent: { seq: 0, messages: [], config: null, tasks: [], rules: [], usage: null },
  goal: null,
  tab: "overview",
};

const $ = (id) => document.getElementById(id);

function t(key) {
  return TEXT[key] || key;
}

function toast(msg, bad) {
  const el = $("toast");
  el.textContent = msg;
  el.style.borderColor = bad ? "var(--bad)" : "#12617d";
  el.classList.remove("hidden");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => el.classList.add("hidden"), 2600);
}

async function api(path, opts) {
  opts = opts || {};
  const headers = Object.assign({ "Content-Type": "application/json" }, opts.headers || {});
  if (state.pin) headers["X-Portal-Pin"] = state.pin;
  const res = await fetch(path, {
    method: opts.method || "GET",
    headers,
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (e) { data = { raw: text }; }
  if (res.status === 401 && data && data.pin_required) {
    askPin();
    throw new Error("pin required");
  }
  return { status: res.status, data };
}

function askPin() {
  $("pin-gate").classList.remove("hidden");
  $("pin-input").value = "";
  $("pin-input").focus();
}

function kv(el, rows) {
  el.innerHTML = "";
  rows.forEach(([k, v]) => {
    if (v === undefined || v === null || v === "") return;
    const row = document.createElement("div");
    row.className = "row";
    const a = document.createElement("span");
    a.textContent = k;
    const b = document.createElement("span");
    b.className = "mono";
    b.textContent = String(v);
    row.append(a, b);
    el.append(row);
  });
}

function badge(text, cls) {
  const s = document.createElement("span");
  s.className = "badge " + (cls || "");
  s.textContent = text;
  return s;
}

async function refreshStatus() {
  const { data } = await api("/api/status");
  if (!data) return;
  state.status = data;
  $("device-name").textContent = data.device_name;
  $("board-chip").textContent = data.board.profile + " · " + data.board.soc;
  $("ip-chip").textContent = (data.wifi && (data.wifi.ip || data.wifi.ap_ip)) || "no ip";
  $("health-dot").className = "dot " + (data.wifi && data.wifi.state.includes("connected") ? "ok" : "bad");
  $("setup-banner").classList.toggle("hidden", !(data.setup && data.setup.needed));

  kv($("ov-board"), [
    ["Profile", data.board.profile_name],
    ["SoC / flash", data.board.soc + " · " + data.board.flash_mb + " MB"],
    ["PSRAM", data.board.psram_kb + " kB"],
    ["Temperature", data.board.temperature_c != null ? data.board.temperature_c + " °C" : "–"],
    ["Free heap", data.memory.free_heap_kb + " kB"],
    ["Uptime", data.uptime_s + " s"],
    ["Firmware", data.firmware.version + " (" + data.firmware.build + ")"],
  ]);
  kv($("ov-mcp"), [
    ["Endpoint", data.mcp.url],
    ["Port / path", data.mcp.port + " " + data.mcp.path],
    ["Tool style", data.mcp.tools_mode],
    ["Tools visible", data.mcp.tools_visible],
    ["Manifest rev", data.mcp.manifest_revision + " · " + data.mcp.manifest_tools + " tools"],
    ["Sessions", data.mcp.sessions],
    ["Auth", data.mcp.auth_required ? "bearer token" : "open on LAN"],
  ]);
  kv($("ov-devices"), [
    ["Detected", data.devices.count],
    ["Unidentified", data.devices.unknown_addresses],
    ["Probe time", data.devices.last_probe_ms + " ms"],
    ["WiFi", (data.wifi && data.wifi.state) || "–"],
    ["RSSI", data.wifi && data.wifi.rssi_dbm ? data.wifi.rssi_dbm + " dBm" : "–"],
  ]);
  const caps = $("ov-caps");
  caps.innerHTML = "";
  (data.devices.caps || []).forEach((c) => {
    const s = document.createElement("span");
    s.className = "tag";
    s.textContent = c;
    caps.append(s);
  });
  $("tools-mode").value = data.mcp.tools_mode;
  kv($("mcp-url"), [
    ["URL", data.mcp.url],
    ["Portal", (data.setup && data.setup.portal_url) || location.href],
  ]);
}

async function refreshDevices() {
  const { data } = await api("/api/devices");
  if (!data) return;
  state.devices = data.devices || [];
  const table = $("device-table");
  table.innerHTML = "";
  if (!state.devices.length) {
    table.innerHTML = '<p class="hint">' + esc(t("no_devices")) + "</p>";
  } else {
    const html = ['<table><thead><tr>' + [t("col_id"), t("col_device"), t("col_bus"), t("col_caps"), t("col_tools"), t("col_state")]
      .map((h) => `<th>${esc(h)}</th>`).join("") + "</tr></thead><tbody>"];
    state.devices.forEach((d) => {
      html.push(
        `<tr><td class="mono">${esc(d.id)}</td><td>${esc(d.name)}</td>` +
        `<td class="mono">${esc(d.bus)}${d.address != null ? " 0x" + d.address.toString(16).toUpperCase() : ""}` +
        `${d.pins && d.pins.length ? " · pins " + d.pins.join(",") : ""}</td>` +
        `<td>${(d.caps || []).map((c) => `<span class="tag">${esc(c)}</span>`).join(" ")}</td>` +
        `<td class="mono">${(d.tools || []).join(", ")}</td>` +
        `<td>${d.status === "ok" ? '<span class="badge ok">ok</span>' : '<span class="badge warn">' + esc(d.status) + "</span>"}` +
        `${d.note ? "<br><small>" + esc(d.note) + "</small>" : ""}</td></tr>`
      );
    });
    html.push("</tbody></table>");
    table.innerHTML = html.join("");
  }
  kv($("unknown-table"), (data.unidentified || []).map((u) => [u.address, u.note]));

  const cat = await api("/api/catalog");
  const rows = (cat.data || []).map(
    (c) =>
      `<tr><td class="mono">${esc(c.kind)}</td><td>${esc(c.name)}</td><td>${esc(c.detection)}</td>` +
      `<td>${c.status === "implemented" ? '<span class="badge ok">done</span>' : '<span class="badge warn">' + esc(c.status) + "</span>"}</td></tr>`
  );
  $("catalog-table").innerHTML =
    "<table><thead><tr>" + [t("col_id"), t("col_device"), t("col_detection"), t("col_state")]
      .map((h) => `<th>${esc(h)}</th>`).join("") + "</tr></thead><tbody>" + rows.join("") + "</tbody></table>";
}

async function refreshTools() {
  const { data } = await api("/api/tools");
  if (!data) return;
  state.tools = data.tools || [];
  $("tools-summary").textContent = `${data.visible} / ${data.total} · ${data.style_hint}`;
  const html = ['<table><thead><tr>' + [t("col_tool"), t("col_category"), t("col_requires"), t("col_description"), t("col_visible")]
    .map((h) => `<th>${esc(h)}</th>`).join("") + "</tr></thead><tbody>"];
  state.tools.forEach((tl) => {
    html.push(
      `<tr class="${tl.visible ? "" : "hidden-tool"}"><td class="mono">${esc(tl.name)}` +
      `${tl.mutating ? ' <span class="badge warn">mutating</span>' : ""}` +
      `${tl.core ? ' <span class="badge core">core</span>' : ""}</td>` +
      `<td>${esc(tl.category)}</td>` +
      `<td class="mono">${(tl.requires || []).join(", ") || "–"}` +
      `${tl.missing && tl.missing.length ? '<br><small style="color:var(--bad)">missing: ' + esc(tl.missing.join(",")) + "</small>" : ""}</td>` +
      `<td>${esc((tl.description || "").slice(0, 140))}</td>` +
      `<td>${tl.visible ? '<span class="badge ok">yes</span>' : '<span class="badge bad">no</span>'}</td></tr>`
    );
  });
  html.push("</tbody></table>");
  $("tool-table").innerHTML = html.join("");
}

// ---------------------------------------------------------------- agent

function renderAgent() {
  const box = $("agent-transcript");
  box.innerHTML = "";
  const msgs = state.agent.messages || [];
  if (!msgs.length) {
    box.innerHTML = '<p class="hint">' + esc(t("agent_empty")) + "</p>";
    return;
  }
  msgs.forEach((m) => {
    const row = document.createElement("div");
    row.className = "msg " + (m.role || "system");
    const head = document.createElement("div");
    head.className = "msg-head";
    head.textContent = (m.role || "?") + ":" + (m.source ? "  · " + m.source : "");
    const body = document.createElement("div");
    body.className = "msg-body";
    let text = m.text || "";
    if (!text && m.tool_calls && m.tool_calls.length) {
      text = m.tool_calls
        .map((c) => "→ " + (c.name || "?") + "(" + JSON.stringify(c.arguments || {}) + ")")
        .join("\n");
    }
    body.textContent = text || "…";
    // The model's reasoning, as its own folded block: the host agent shows thinking
    // separately from the answer, and so does this.
    if (m.reasoning) {
      const think = document.createElement("details");
      think.className = "msg-think";
      const summary = document.createElement("summary");
      summary.textContent = "thinking · " + m.reasoning.length + " chars";
      const inner = document.createElement("div");
      inner.textContent = m.reasoning;
      think.append(summary, inner);
      row.append(think);
    }
    if (m.is_error) row.classList.add("msg-error");
    row.append(head, body);
    box.append(row);
  });
  const usage = state.agent.usage && state.agent.usage.last_run;
  if (usage && usage.total_tokens) {
    const foot = document.createElement("div");
    foot.className = "hint";
    foot.textContent =
      "tokens: " +
      usage.total_tokens +
      " (prompt " +
      usage.prompt_tokens +
      ", completion " +
      usage.completion_tokens +
      ")";
    box.append(foot);
  }
  box.scrollTop = box.scrollHeight;
}

async function refreshAgentHistory() {
  const { data } = await api("/api/agent/history?after=0");
  if (!data) return;
  state.agent.seq = data.seq || 0;
  state.agent.usage = data.usage || null;
  state.agent.messages = data.messages || [];
  renderAgent();
}

async function refreshAgentConfig() {
  const { data } = await api("/api/agent/config");
  if (!data) return;
  state.agent.config = data;
  $("agent-base-url").value = data.base_url || "";
  $("agent-model").value = data.model || "";
  $("agent-max-steps").value = data.max_steps || 4;
  $("agent-tick").value = data.tick_ms || 1000;
  const key = $("agent-api-key");
  key.value = "";
  key.placeholder = data.api_key_set ? t("agent_key_set") : t("agent_key_none");
  const run = data.run || {};
  $("agent-runs").textContent =
    (run.busy || run.pending ? t("agent_running") : t("agent_idle")) +
    " · runs " + (run.runs || 0) + " · " + (run.history || 0) + " msgs";
  $("agent-hint").textContent = run.last_error ? "last_error: " + run.last_error : "";
  kv($("agent-config-state"), [
    ["base_url", data.base_url || "–"],
    ["model", data.model || "–"],
    ["api_key", data.api_key_set ? "__set__" : "–"],
    ["max_steps", data.max_steps],
    ["tick_ms", (data.tick_ms || 1000) + " ms"],
    ["temperature", data.temperature],
    ["timeout_s", (data.timeout_s || 0) + " s"],
    ["history", run.history || 0],
    ["pending", run.pending ? "queued" : "–"],
  ]);
}

function clampTick(ms) {
  if (!Number.isFinite(ms) || ms <= 0) return 1000;
  return Math.min(5000, Math.max(200, Math.round(ms)));
}

async function sendAgentMessage() {
  const el = $("agent-text");
  const text = el.value.trim();
  if (!text) return;
  const { status, data } = await api("/api/agent/message", { method: "POST", body: { text } });
  if (status !== 202) {
    toast((data && data.error) || t("failed"), true);
    return;
  }
  el.value = "";
  toast(t("agent_queued"));
  setTimeout(() => {
    refreshAgentHistory().catch(() => {});
    refreshAgentConfig().catch(() => {});
  }, 700);
}

async function resetAgent() {
  if (!confirm("Clear the transcript?")) return;
  const { status } = await api("/api/agent/reset", { method: "POST" });
  if (status !== 200) return toast(t("failed"), true);
  state.agent.messages = [];
  state.agent.seq = 0;
  renderAgent();
  toast(t("saved"));
}

async function saveAgentConfig() {
  const body = {
    base_url: $("agent-base-url").value.trim(),
    model: $("agent-model").value.trim(),
    max_steps: Number($("agent-max-steps").value) || 4,
    // Clamped here as well as on the board, so the field never shows a value the
    // board would silently replace.
    tick_ms: clampTick(Number($("agent-tick").value)),
  };
  $("agent-tick").value = body.tick_ms;
  const key = $("agent-api-key").value;
  if (key) body.api_key = key; // empty field = keep the stored key
  const { status, data } = await api("/api/agent/config", { method: "POST", body });
  toast(status === 200 ? t("saved") : (data && data.error) || t("failed"), status !== 200);
  if (status === 200) refreshAgentConfig().catch(() => {});
}

// ---------------------------------------------------------------- goal loop
// One objective the board works through on its own. The card never drives the loop
// itself: the agent thread does, and this only starts/stops it and shows
// GET /api/goal, which is the board's own record of where the loop is.

const GOAL_BADGE = { running: "core", met: "ok", blocked: "bad", exhausted: "warn", stopped: "warn" };

function goalStatusLabel(status) {
  const key = "goal_status_" + status;
  const label = t(key);
  return label === key ? status : label;
}

function renderGoalNotes(notes) {
  const box = $("goal-notes");
  box.innerHTML = "";
  const label = document.createElement("p");
  label.className = "hint";
  label.textContent = t("goal_notes");
  box.append(label);
  const list = document.createElement("div");
  list.className = "list";
  if (!notes.length) {
    list.innerHTML = '<p class="hint">' + esc(t("goal_no_notes")) + "</p>";
    box.append(list);
    return;
  }
  notes.forEach((note, i) => {
    const item = document.createElement("div");
    item.className = "list-item";
    // A note is text, not a target: the hover border is for the WiFi list.
    item.style.cursor = "default";
    const n = document.createElement("span");
    n.className = "mono";
    n.textContent = String(i + 1);
    const text = document.createElement("span");
    text.style.flex = "1";
    text.textContent = note;
    item.append(n, text);
    list.append(item);
  });
  box.append(list);
}

function renderGoal() {
  const data = state.goal || {};
  const status = String(data.status || "idle");
  const active = status === "running";
  const iteration = Number(data.iteration) || 0;
  const maxIterations = Number(data.max_iterations) || 0;

  const badge = $("goal-badge");
  badge.className = "badge " + (GOAL_BADGE[status] || "off");
  badge.textContent = goalStatusLabel(status);

  // The board answers 409 to a second goal and leaves a finished one untouched by a
  // stop, so the buttons only offer what the board will actually do.
  $("goal-start").disabled = active;
  $("goal-stop").disabled = !active;

  const box = $("goal-state");
  if (!data.objective) {
    box.innerHTML = '<p class="hint">' + esc(t("goal_idle")) + "</p>";
  } else {
    // One line carries both facts: which round the loop is on, and where it stands.
    // kv drops empty values, so every row here needs a value to be drawn at all.
    kv(box, [
      [t("goal_objective"), data.objective],
      [t("goal_iter").replace("{i}", iteration).replace("{m}", maxIterations), goalStatusLabel(status)],
      [t("goal_failures"), data.failures ? data.failures : ""],
      [t("goal_last"), data.last_result],
      [t("goal_reason"), data.stop_reason],
    ]);
  }
  renderGoalNotes(data.notes || []);
}

async function refreshGoal() {
  const { data } = await api("/api/goal");
  if (!data) return;
  // This runs on every 4 s poll. Redraw only when the board's document actually
  // changed, so a text selection inside the notes survives the next tick.
  if (JSON.stringify(data) === JSON.stringify(state.goal)) return;
  state.goal = data;
  renderGoal();
}

function clampGoalIterations(n) {
  if (!Number.isFinite(n) || n <= 0) return 10;
  return Math.min(100, Math.max(1, Math.round(n)));
}

async function startGoal() {
  const objective = $("goal-objective").value.trim();
  if (!objective) return toast(t("goal_need_objective"), true);
  // Clamped here as well as on the board, so the field never shows a value the
  // board would silently replace.
  const maxIterations = clampGoalIterations(Number($("goal-max-iterations").value));
  $("goal-max-iterations").value = maxIterations;
  const { status, data } = await api("/api/goal", {
    method: "POST",
    body: { objective, max_iterations: maxIterations },
  });
  if (status !== 200) {
    toast((data && data.error) || t("failed"), true);
    // A 409 carries the goal that is already running: show it instead of a stale one.
    if (data && data.goal) {
      state.goal = data.goal;
      renderGoal();
    }
    return;
  }
  $("goal-objective").value = "";
  state.goal = data;
  renderGoal();
  toast(t("goal_started"));
}

async function stopGoal() {
  // No confirm: the board keeps the objective and the notes, and the badge says what
  // happened, so a stop costs a re-statement, not the record.
  const { status, data } = await api("/api/goal/stop", { method: "POST", body: {} });
  if (status !== 200) {
    toast((data && data.error) || t("failed"), true);
    return;
  }
  state.goal = data;
  renderGoal();
  toast(t("goal_status_stopped"));
}

function schedButton(label, onClick) {
  const b = document.createElement("button");
  b.className = "btn ghost";
  b.textContent = label;
  b.onclick = onClick;
  return b;
}

function actionCell(buttons) {
  const td = document.createElement("td");
  td.className = "row";
  buttons.forEach((b) => td.append(b));
  return td;
}

function schedTable(head) {
  const table = document.createElement("table");
  const thead = document.createElement("thead");
  const tr = document.createElement("tr");
  head.forEach((h) => {
    const th = document.createElement("th");
    th.textContent = h;
    tr.append(th);
  });
  thead.append(tr);
  table.append(thead, document.createElement("tbody"));
  return table;
}

function schedRow(table, cells, actions) {
  const tr = document.createElement("tr");
  cells.forEach((c, i) => {
    const td = document.createElement("td");
    if (i === 0) td.className = "mono";
    td.textContent = String(c == null ? "" : c);
    tr.append(td);
  });
  tr.append(actionCell(actions));
  table.querySelector("tbody").append(tr);
}

function renderTasks() {
  const box = $("sched-task-table");
  box.innerHTML = "";
  const tasks = state.agent.tasks || [];
  if (!tasks.length) {
    box.innerHTML = '<p class="hint">' + esc(t("none")) + "</p>";
    return;
  }
  const table = schedTable(["id", "trigger", "prompt", "state", ""]);
  tasks.forEach((task) => {
    schedRow(
      table,
      [
        task.id,
        task.every_s ? "every " + task.every_s + " s" : task.at_hhmm || "–",
        task.prompt,
        task.enabled ? "on" : "off",
      ],
      [
        schedButton(task.enabled ? t("sched_disable") : t("sched_enable"), () =>
          schedPost({ op: task.enabled ? "disable" : "enable", kind: "task", id: task.id })
        ),
        schedButton(t("del"), () => schedPost({ op: "delete", kind: "task", id: task.id })),
      ]
    );
  });
  box.append(table);
}

function renderRules() {
  const box = $("sched-rule-table");
  box.innerHTML = "";
  const rules = state.agent.rules || [];
  if (!rules.length) {
    box.innerHTML = '<p class="hint">' + esc(t("none")) + "</p>";
    return;
  }
  const table = schedTable(["id", "tool", "path", "op", "value", "min s", "state", ""]);
  rules.forEach((rule) => {
    schedRow(
      table,
      [
        rule.id,
        rule.tool,
        rule.path || "–",
        rule.op,
        JSON.stringify(rule.value),
        rule.min_interval_s,
        rule.enabled ? "on" : "off",
      ],
      [
        schedButton(rule.enabled ? t("sched_disable") : t("sched_enable"), () =>
          schedPost({ op: rule.enabled ? "disable" : "enable", kind: "rule", id: rule.id })
        ),
        schedButton(t("del"), () => schedPost({ op: "delete", kind: "rule", id: rule.id })),
      ]
    );
  });
  box.append(table);
}

async function refreshSched() {
  const { data } = await api("/api/sched");
  if (!data) return;
  state.agent.tasks = data.tasks || [];
  state.agent.rules = data.rules || [];
  renderTasks();
  renderRules();
}

async function schedPost(body) {
  const { status, data } = await api("/api/sched", { method: "POST", body });
  if (status !== 200) {
    toast((data && data.error) || t("failed"), true);
    return;
  }
  state.agent.tasks = data.tasks || [];
  state.agent.rules = data.rules || [];
  renderTasks();
  renderRules();
  toast(t("saved"));
}

function ruleValue(raw) {
  const s = String(raw).trim();
  if (s === "true") return true;
  if (s === "false") return false;
  if (s && !isNaN(Number(s))) return Number(s);
  return s;
}

function addTask() {
  const id = $("task-id").value.trim() || "t" + (state.agent.tasks.length + 1);
  const prompt = $("task-prompt").value.trim();
  if (!prompt) return toast(t("need_prompt"), true);
  schedPost({
    op: "upsert",
    kind: "task",
    task: {
      id,
      prompt,
      every_s: Number($("task-every").value) || 0,
      at_hhmm: $("task-at").value.trim(),
      enabled: true,
    },
  }).then(() => {
    $("task-prompt").value = "";
    $("task-every").value = "";
    $("task-at").value = "";
    $("task-id").value = "";
  });
}

function addRule() {
  const tool = $("rule-tool").value.trim();
  if (!tool) return toast(t("need_tool"), true);
  const id = $("rule-id").value.trim() || "r" + (state.agent.rules.length + 1);
  schedPost({
    op: "upsert",
    kind: "rule",
    rule: {
      id,
      tool,
      path: $("rule-path").value.trim(),
      op: $("rule-op").value,
      value: ruleValue($("rule-value").value),
      prompt: $("rule-prompt").value.trim() || t("default_rule_prompt"),
      min_interval_s: Number($("rule-every").value) || 60,
      enabled: true,
    },
  }).then(() => {
    ["rule-id", "rule-tool", "rule-path", "rule-value", "rule-prompt", "rule-every"].forEach(
      (k) => ($(k).value = "")
    );
  });
}

// ---------------------------------------------------------------- live journal
// /api/agent/stream is server-sent events: one frame per journal entry, with `id`
// and `event` set so a plain EventSource can do the work. The board ends the
// stream on purpose (30 s silent, or 120 s in total) and EventSource reconnects by
// itself, sending Last-Event-ID; the `after=` in the URL is only the starting
// point, so this page never resends or replays an event on its own.

const STREAM_KINDS = [
  "message_start", "thinking_start", "thinking_delta", "thinking_end",
  "text_start", "text_delta", "text_end",
  "tool_use", "tool_progress", "tool_result",
  "usage", "message_end", "error", "run_end",
];
const STREAM_MAX_LINES = 200;
const STREAM_MAX_CHARS = 240;

const stream = {
  es: null,
  state: "off", // off | connecting | live | offline
  lastSeq: 0,
  count: 0,
  skipped: 0,
  paused: false,
  lastAt: 0,
  timer: null,
  backoff: 3000,
};

function stampNow() {
  const d = new Date();
  const p = (n, w) => String(n).padStart(w || 2, "0");
  return p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds()) + "." + p(d.getMilliseconds(), 3);
}

/// The gap to the previous event, from the board's own monotonic clock: a run that
/// takes 40 s and one that takes 4 s look the same otherwise.
function streamDelta(ev) {
  const at = Number(ev.at_ms);
  if (!Number.isFinite(at)) return "";
  const out = stream.lastAt && at >= stream.lastAt ? "+" + ((at - stream.lastAt) / 1000).toFixed(2) + "s" : "";
  stream.lastAt = at;
  return out;
}

function streamClass(kind) {
  if (kind === "tool_progress") return "progress";
  if (kind === "tool_use" || kind === "tool_result") return "tool";
  if (kind.indexOf("thinking_") === 0) return "thinking";
  if (kind.indexOf("text_") === 0) return "text";
  if (kind === "usage") return "usage";
  if (kind === "error") return "error";
  if (kind === "run_end") return "run";
  return "mark";
}

/// One line of the log. Every field the board sends has a place, and an event kind
/// this page does not know still shows up raw instead of vanishing.
function streamBody(ev) {
  const kind = String(ev.event || "");
  let text = "";
  if (kind === "tool_use") {
    text = (ev.name || "?") + "(" + (ev.text || "{}") + ")";
  } else if (kind === "tool_progress") {
    text = (ev.name || "?") + " · " + (ev.phase || "?") +
      (ev.elapsed_ms != null ? " · " + ev.elapsed_ms + " ms" : "");
  } else if (kind === "tool_result") {
    text = (ev.name ? ev.name + " → " : "") + (ev.text || (ev.is_error ? "error" : "ok"));
  } else if (kind === "usage") {
    text = "prompt " + (ev.prompt_tokens || 0) + " · completion " + (ev.completion_tokens || 0) +
      " · total " + (ev.total_tokens || 0);
  } else {
    text = ev.text || "";
  }
  const full = String(text);
  if (full.length > STREAM_MAX_CHARS) return full.slice(0, STREAM_MAX_CHARS) + "…";
  return full;
}

function streamAppend(ev) {
  const box = $("agent-stream");
  if (!box || !ev) return;
  const kind = String(ev.event || "?");
  if (stream.paused) {
    stream.skipped += 1;
    syncStreamUi();
    return;
  }
  if (box.dataset.empty === "1") {
    box.innerHTML = "";
    delete box.dataset.empty;
  }
  // Only follow the tail when the reader is already there; scrolling up to read
  // must not be yanked back by the next event.
  const atBottom = box.scrollTop + box.clientHeight >= box.scrollHeight - 24;
  const line = document.createElement("div");
  line.className = "ev ev-" + streamClass(kind);
  const ts = document.createElement("span");
  ts.className = "ts";
  ts.textContent = stampNow();
  const dt = document.createElement("span");
  dt.className = "dt";
  dt.textContent = streamDelta(ev);
  const name = document.createElement("span");
  name.className = "kind";
  name.textContent = kind;
  const msg = document.createElement("span");
  msg.className = "msg";
  msg.textContent = streamBody(ev);
  line.append(ts, dt, name, msg);
  line.title = JSON.stringify(ev);
  box.append(line);
  while (box.childElementCount > STREAM_MAX_LINES) box.removeChild(box.firstElementChild);
  if (atBottom) box.scrollTop = box.scrollHeight;
  stream.count += 1;
  syncStreamUi();
}

function onStreamEvent(frame) {
  // `error` is both a journal kind and the browser's own connection-error event: a
  // frame with no data string is the latter, and belongs to onerror, not to the log.
  if (!frame || typeof frame.data !== "string" || !frame.data) return;
  let ev = null;
  try { ev = JSON.parse(frame.data); } catch (e) { return; }
  // A frame on the wire is proof the stream is up: don't leave the badge on
  // "connecting" just because `open` has not been dispatched yet.
  if (ev && stream.state !== "live") setStreamState("live");
  if (ev && typeof ev.seq === "number" && ev.seq >= stream.lastSeq) stream.lastSeq = ev.seq;
  streamAppend(ev);
  const kind = ev && ev.event;
  if (kind === "error") toast(String(ev.text || "run failed").slice(0, 140), true);
  if (kind === "message_end" || kind === "run_end" || kind === "error") {
    // The journal says where the run is; the transcript is still the readable
    // shape of it, so refresh it once per message instead of polling.
    refreshAgentHistory().catch(() => {});
    if (kind === "run_end" || kind === "error") refreshAgentConfig().catch(() => {});
  }
}

function setStreamState(next) {
  stream.state = next;
  syncStreamUi();
}

function streamOpen() {
  if (stream.es) return;
  clearTimeout(stream.timer);
  stream.timer = null;
  const es = new EventSource("/api/agent/stream?after=" + (stream.lastSeq || 0));
  stream.es = es;
  setStreamState("connecting");
  es.onopen = () => {
    if (stream.es !== es) return; // a replaced stream cannot speak for this one
    stream.backoff = 3000;
    setStreamState("live");
  };
  es.onerror = () => {
    // 2 is CLOSED in every implementation of the spec; compared as a number so a
    // missing static (or a shim) cannot turn a refusal into an endless retry.
    if (stream.es !== es) {
      try { es.close(); } catch (err) { /* already gone */ }
      return;
    }
    if (es.readyState === 2) {
      // Not a transient drop: the board refused the stream (it allows two) or the
      // endpoint is gone. EventSource gives up for good on that, so retry slowly —
      // but close this object first, so nothing keeps reconnecting behind our back.
      try { es.close(); } catch (err) { /* already gone */ }
      stream.es = null;
      setStreamState("offline");
      scheduleStreamReopen();
    } else {
      setStreamState("connecting"); // EventSource is already retrying
    }
  };
  STREAM_KINDS.forEach((kind) =>
    es.addEventListener(kind, (frame) => {
      // Frames from a superseded stream are dropped: only the current one may
      // advance the log, or a stale socket could replay the run a second time.
      if (stream.es !== es) return;
      onStreamEvent(frame);
    })
  );
}

function scheduleStreamReopen() {
  if (state.tab !== "agent" || document.hidden) return;
  clearTimeout(stream.timer);
  stream.timer = setTimeout(() => {
    stream.timer = null;
    streamOpen();
    stream.backoff = Math.min(stream.backoff * 2, 30000);
  }, stream.backoff);
}

/// Close the connection, not the panel: the board serves six connections and two
/// streams is its whole budget, so leaving the tab gives one back.
function streamClose() {
  clearTimeout(stream.timer);
  stream.timer = null;
  if (stream.es) {
    stream.es.close();
    stream.es = null;
  }
  setStreamState("off");
}

function toggleStreamPause() {
  stream.paused = !stream.paused;
  // The skipped count describes the pause that just ended, not the log as a whole.
  if (!stream.paused) stream.skipped = 0;
  syncStreamUi();
}

function streamClear() {
  const box = $("agent-stream");
  stream.count = 0;
  stream.skipped = 0;
  stream.lastAt = 0;
  if (box) {
    box.innerHTML = '<p class="hint">' + esc(t("stream_empty")) + "</p>";
    box.dataset.empty = "1";
  }
  syncStreamUi();
}

function syncStreamUi() {
  const badge = $("agent-stream-badge");
  if (!badge) return;
  // Pause outranks the transport state: it is what the user did, and it is what
  // governs the log. Where the socket is while paused goes in the note instead of
  // hiding either fact.
  let cls = "off";
  let label = t("stream_offline");
  if (stream.paused) {
    cls = "paused";
    label = t("stream_paused");
  } else if (stream.state === "live") {
    cls = "live";
    label = t("stream_live");
  } else if (stream.state === "connecting") {
    cls = "retry";
    label = t("stream_connecting");
  } else if (stream.state === "offline") {
    cls = "bad";
    label = t("stream_offline");
  }
  badge.className = "badge " + cls;
  badge.textContent = label;
  const note = $("agent-stream-note");
  if (note) {
    let text = "seq " + stream.lastSeq + " · " + stream.count + " ev";
    if (stream.paused) {
      if (stream.state === "connecting") text += " · " + t("stream_connecting");
      else if (stream.state !== "live") text += " · " + t("stream_offline");
      if (stream.skipped) text += " · " + t("stream_skipped").replace("{n}", stream.skipped);
    }
    note.textContent = text;
  }
  const pause = $("agent-stream-pause");
  if (pause) pause.textContent = stream.paused ? t("stream_resume") : t("stream_pause");
}

async function refreshAgent() {
  await refreshAgentConfig().catch(() => {});
  await refreshAgentHistory().catch(() => {});
  await refreshSched().catch(() => {});
  await refreshGoal().catch(() => {});
}

async function refreshManifest() {
  const { data } = await api("/api/manifest");
  if (!data) return;
  state.manifest = data;
  $("manifest-text").value = JSON.stringify(data.manifest, null, 2);
}

async function refreshClients() {
  const { data } = await api("/api/clients");
  if (!data) return;
  kv($("mcp-url"), [["URL", data.url]]);
  $("curl-list").textContent = data.curl_list;
  $("mcp-json").textContent = JSON.stringify(data.mcp_servers_json, null, 2);
}

async function refreshLogs() {
  const { data } = await api("/api/logs?n=200");
  if (!data) return;
  const lines = (data.lines || []).map(
    (l) => `${String(l.t_ms).padStart(8)} ${l.level.padEnd(5)} ${l.target.padEnd(10)} ${l.msg}`
  );
  $("log-box").textContent = lines.join("\n") || "(empty)";
  $("log-box").scrollTop = $("log-box").scrollHeight;
}

async function scanWifi() {
  const btn = $("wifi-scan");
  btn.disabled = true;
  try {
    const { data } = await api("/api/wifi/scan");
    const list = $("wifi-list");
    list.innerHTML = "";
    ((data && data.networks) || []).forEach((n) => {
      const item = document.createElement("div");
      item.className = "list-item";
      const left = document.createElement("span");
      left.textContent = `${n.ssid || "(hidden)"} · ${n.auth}`;
      const right = document.createElement("span");
      right.className = "mono";
      right.textContent = `${n.rssi} dBm · ch${n.channel}`;
      item.append(left, right);
      item.onclick = () => { $("wifi-ssid").value = n.ssid; $("wifi-password").focus(); };
      list.append(item);
    });
    if (!list.children.length) list.innerHTML = '<p class="hint">' + esc(t("no_networks")) + "</p>";
  } finally {
    btn.disabled = false;
  }
}

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

function showTab(name) {
  state.tab = name;
  document.querySelectorAll(".tab").forEach((b) => b.classList.toggle("active", b.dataset.tab === name));
  document.querySelectorAll(".panel").forEach((p) => p.classList.toggle("active", p.id === "panel-" + name));
  localStorage.setItem("tab", name);
  if (name === "devices") refreshDevices();
  if (name === "tools") refreshTools();
  if (name === "manifest") refreshManifest();
  if (name === "agent") {
    refreshAgent();
    streamOpen();
  } else {
    streamClose();
  }
  if (name === "clients") refreshClients();
  if (name === "logs") refreshLogs();
}

function renderIssues(issues) {
  const box = $("manifest-issues");
  box.innerHTML = "";
  (issues || []).forEach((i) => {
    const div = document.createElement("div");
    div.className = "issue";
    div.innerHTML = `<span class="path">${esc(i.path)}</span> — ${esc(i.message)}` +
      (i.hint ? `<br><small>${esc(i.hint)}</small>` : "");
    box.append(div);
  });
}

function bind() {
  $("tabs").addEventListener("click", (e) => {
    const b = e.target.closest(".tab");
    if (b) showTab(b.dataset.tab);
  });
  $("pin-submit").addEventListener("click", async () => {
    state.pin = $("pin-input").value.trim();
    localStorage.setItem("pin", state.pin);
    $("pin-gate").classList.add("hidden");
    toast("PIN ok");
    refreshStatus();
  });
  $("reboot-btn").addEventListener("click", async () => {
    if (!confirm("Reboot the board?")) return;
    await api("/api/reboot", { method: "POST" });
    toast("rebooting");
  });
  $("wifi-scan").addEventListener("click", scanWifi);
  $("wifi-connect").addEventListener("click", async () => {
    const ssid = $("wifi-ssid").value.trim();
    if (!ssid) return toast(t("need_ssid"), true);
    const { status, data } = await api("/api/wifi/connect", {
      method: "POST",
      body: { ssid, password: $("wifi-password").value },
    });
    toast(status === 200 ? (data.note || t("saved")) : (data && data.error) || t("failed"), status !== 200);
    setTimeout(refreshStatus, 1500);
  });
  $("wifi-forget").addEventListener("click", async () => {
    if (!confirm("Clear the stored WiFi credentials?")) return;
    await api("/api/wifi/forget", { method: "POST" });
    toast(t("saved"));
    refreshStatus();
  });
  $("devices-probe").addEventListener("click", async () => {
    const { data } = await api("/api/devices/probe", { method: "POST" });
    refreshDevices();
    toast(`re-probed in ${data && data.took_ms} ms`);
  });
  $("tools-mode").addEventListener("change", async (e) => {
    await api("/api/tools", { method: "POST", body: { mode: e.target.value } });
    refreshTools();
    refreshStatus();
  });
  $("manifest-validate").addEventListener("click", async () => {
    let doc;
    try { doc = JSON.parse($("manifest-text").value); } catch (err) {
      return renderIssues([{ path: "(json)", message: t("bad_json") + err.message, hint: "" }]);
    }
    const { data } = await api("/api/manifest", { method: "POST", body: { manifest: doc, dry_run: true } });
    renderIssues((data && (data.issues || [])) || []);
    toast(data && data.applied === false ? "checked" : t("applied"));
  });
  $("manifest-apply").addEventListener("click", async () => {
    let doc;
    try { doc = JSON.parse($("manifest-text").value); } catch (err) {
      return renderIssues([{ path: "(json)", message: t("bad_json") + err.message, hint: "" }]);
    }
    const mode = $("manifest-mode").value;
    const { status, data } = await api("/api/manifest", {
      method: "POST",
      body: { manifest: doc, mode },
    });
    renderIssues((data && data.issues) || []);
    if (status === 200 && data && data.applied) {
      toast(t("applied") + " · rev " + data.revision);
      await refreshStatus();
      await refreshDevices();
      await refreshTools();
      await refreshManifest();
    } else {
      toast((data && data.error) || "issues found", true);
    }
  });
  $("logs-refresh").addEventListener("click", refreshLogs);
  $("agent-send").addEventListener("click", sendAgentMessage);
  $("agent-text").addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") sendAgentMessage();
  });
  $("agent-reset").addEventListener("click", resetAgent);
  $("agent-stream-pause").addEventListener("click", toggleStreamPause);
  $("agent-stream-clear").addEventListener("click", streamClear);
  $("agent-config-save").addEventListener("click", saveAgentConfig);
  $("goal-start").addEventListener("click", startGoal);
  $("goal-stop").addEventListener("click", stopGoal);
  $("goal-objective").addEventListener("keydown", (e) => {
    // Starting a goal is the one thing this field can do, so Enter does it.
    if (e.key === "Enter") startGoal();
  });
  $("sched-refresh").addEventListener("click", refreshSched);
  $("sched-task-add").addEventListener("click", addTask);
  $("sched-rule-add").addEventListener("click", addRule);
}

(async function main() {
  bind();
  showTab(localStorage.getItem("tab") || "overview");
  try {
    await refreshStatus();
    await refreshTools();
    if (state.status && state.status.setup.needed) {
      $("setup-banner").classList.remove("hidden");
      await scanWifi();
    }
  } catch (e) {
    console.error(e);
  }
  setInterval(() => {
    refreshStatus().catch(() => {});
  }, 5000);
  setInterval(() => {
    if (state.tab !== "agent") return;
    refreshSched().catch(() => {});
    // The loop advances one iteration per tick on the board, so the card follows the
    // same 4 s poll the schedule does, not the stream.
    refreshGoal().catch(() => {});
    // While the stream is up it carries the run as it happens; the transcript is
    // refreshed on message_end instead of on a timer.
    if (stream.state === "live") return;
    refreshAgentHistory().catch(() => {});
    refreshAgentConfig().catch(() => {});
  }, 4000);
})();
