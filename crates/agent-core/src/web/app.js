// esp32-agent portal. No build step, no framework: the board serves this file.
"use strict";

const I18N = {
  zh: {
    reboot: "重启", tab_overview: "总览", tab_wifi: "WiFi", tab_devices: "设备",
    tab_tools: "工具", tab_manifest: "清单", tab_agent: "智能体", tab_clients: "接入", tab_logs: "日志",
    setup_title: "设备还没有加入网络",
    setup_hint: "用手机连接设备自己发出的 WiFi（默认 esp32-agent-setup），然后在这个页面填写家里的 WiFi，保存后设备会自动联网。",
    card_board: "板子", card_mcp: "MCP 服务", card_devices: "周边设备", card_caps: "能力",
    card_flow: "给大模型的工作流",
    flow_1: "发现：inventory / i2c_scan / i2c_identify 找到真实接线。",
    flow_2: "建模：manifest_describe 拿到清单 schema、原语和示例。",
    flow_3: "写入：manifest_apply 校验并生效，无需重启、无需重烧。",
    flow_4: "验证：tools/list 里出现新工具，直接调用。",
    flow_5: "迭代：结果不对就改清单再应用，工具列表实时更新。",
    wifi_current: "当前连接", wifi_scan: "扫描网络", wifi_forget: "忘记网络",
    wifi_join: "连接新的 WiFi", wifi_connect: "保存并连接",
    wifi_note: "凭证只写入板子的 NVS，不会进固件；换网络不用重新烧录。",
    devices_found: "识别到的设备", devices_probe: "重新探测",
    devices_unknown: "未识别的 I2C 地址", devices_catalog: "支持的驱动",
    tools_listed: "暴露给模型的工具", tools_mode: "风格",
    manifest_title: "设备清单（manifest）", manifest_validate: "校验",
    manifest_apply: "应用",
    manifest_note: "清单存在 NVS 里，应用后立即生效：工具列表会重建，不需要重启也不需要重新烧录。",
    clients_url: "MCP 端点",
    clients_note: "浏览器或 Claude Desktop 需要支持 Streamable HTTP；只支持 stdio 的客户端要走 mcp-remote 之类的桥。",
    logs_title: "运行日志", refresh: "刷新",
    pin_title: "需要 PIN", pin_hint: "这个板子设置了控制台 PIN，输入后才能修改配置。",
    save: "保存",
    agent_link: "模型连接", agent_chat: "对话", agent_reset: "清空", agent_send: "发送",
    agent_max_steps: "最大步数",
    agent_note: "模型地址必须是 http://（板子上没有 TLS）；key 只写进 NVS，读回来时只显示是否已设置。",
    agent_key_set: "已设置，留空表示不改", agent_key_none: "没有 key（不发送 Authorization）",
    agent_queued: "已排队", agent_running: "运行中", agent_idle: "空闲", agent_empty: "还没有对话。发一条消息，模型会用板子自己的工具作答。",
    sched_tasks: "定时任务", sched_rules: "传感器规则", sched_add: "添加",
    sched_enable: "启用", sched_disable: "停用", del: "删除", none: "（空）",
    sched_note: "“每 N 秒”和“HH:MM”二选一：填秒数按周期触发，填 HH:MM 每天触发一次；到点的提示词会排进同一条智能体队列。",
    sched_rule_note: "规则每轮先调用工具读一次值，命中才排提示词；min_interval_s 防止一条规则持续刷屏。",
    saved: "已保存", applied: "已应用", failed: "失败",
  },
  en: {
    reboot: "Reboot", tab_overview: "Overview", tab_wifi: "WiFi", tab_devices: "Devices",
    tab_tools: "Tools", tab_manifest: "Manifest", tab_agent: "Agent", tab_clients: "Clients", tab_logs: "Logs",
    setup_title: "This board is not on a network yet",
    setup_hint: "Join the WiFi network the board is broadcasting (esp32-agent-setup by default), then enter your home network here. The board joins it and keeps the setting in NVS.",
    card_board: "Board", card_mcp: "MCP service", card_devices: "Peripherals", card_caps: "Capabilities",
    card_flow: "How a model configures this board",
    flow_1: "Discover: inventory / i2c_scan / i2c_identify.",
    flow_2: "Model: manifest_describe gives the schema, primitives and examples.",
    flow_3: "Write: manifest_apply validates and applies — no reboot, no reflash.",
    flow_4: "Verify: the new tools appear in tools/list and work immediately.",
    flow_5: "Iterate: edit and apply again as often as needed.",
    wifi_current: "Current link", wifi_scan: "Scan", wifi_forget: "Forget",
    wifi_join: "Join a network", wifi_connect: "Save and connect",
    wifi_note: "Credentials are stored in NVS only; they are never compiled into the firmware.",
    devices_found: "Detected devices", devices_probe: "Re-probe",
    devices_unknown: "Unidentified I2C addresses", devices_catalog: "Supported drivers",
    tools_listed: "Tools exposed to the model", tools_mode: "Style",
    manifest_title: "Device manifest", manifest_validate: "Validate",
    manifest_apply: "Apply",
    manifest_note: "Stored in NVS and applied live: the tool list is rebuilt in place — no reboot, no reflash.",
    clients_url: "MCP endpoint",
    clients_note: "Clients must speak the Streamable HTTP transport; stdio-only clients need a bridge such as mcp-remote.",
    logs_title: "Log", refresh: "Refresh",
    pin_title: "PIN required", pin_hint: "This board protects its console with a PIN.",
    save: "Save",
    agent_link: "Model link", agent_chat: "Conversation", agent_reset: "Clear", agent_send: "Send",
    agent_max_steps: "Max steps",
    agent_note: "The endpoint must be http:// — the board has no TLS. The key is stored in NVS and never read back.",
    agent_key_set: "set, leave empty to keep it", agent_key_none: "no key (no Authorization header)",
    agent_queued: "Queued", agent_running: "running", agent_idle: "idle", agent_empty: "No conversation yet. Send a message and the model works with the board's own tools.",
    sched_tasks: "Scheduled tasks", sched_rules: "Sensor rules", sched_add: "Add",
    sched_enable: "Enable", sched_disable: "Disable", del: "Delete", none: "(none)",
    sched_note: "Use either every-N-seconds or HH:MM: the first repeats, the second fires once a day. A due prompt joins the same agent queue.",
    sched_rule_note: "Each tick reads the tool once and only queues a prompt when the rule matches; min_interval_s keeps one rule from flooding the queue.",
    saved: "Saved", applied: "Applied", failed: "Failed",
  },
};

const state = {
  lang: localStorage.getItem("lang") || "zh",
  pin: localStorage.getItem("pin") || "",
  status: null,
  config: null,
  devices: [],
  tools: [],
  manifest: { revision: 0, manifest: { version: 1, devices: [], tools: [] } },
  agent: { seq: 0, messages: [], config: null, tasks: [], rules: [] },
  tab: "overview",
};

const $ = (id) => document.getElementById(id);

function t(key) {
  return (I18N[state.lang] && I18N[state.lang][key]) || I18N.zh[key] || key;
}

function applyLang() {
  document.documentElement.lang = state.lang === "zh" ? "zh-CN" : "en";
  document.querySelectorAll("[data-i18n]").forEach((el) => {
    el.textContent = t(el.getAttribute("data-i18n"));
  });
  $("lang-btn").textContent = state.lang === "zh" ? "EN" : "中";
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
    table.innerHTML = '<p class="hint">没有识别到设备。接好线后点“重新探测”。</p>';
  } else {
    const html = ['<table><thead><tr><th>id</th><th>设备</th><th>总线</th><th>能力</th><th>工具</th><th>状态</th></tr></thead><tbody>'];
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
    '<table><thead><tr><th>id</th><th>设备</th><th>识别方式</th><th>状态</th></tr></thead><tbody>' + rows.join("") + "</tbody></table>";
}

async function refreshTools() {
  const { data } = await api("/api/tools");
  if (!data) return;
  state.tools = data.tools || [];
  $("tools-summary").textContent = `${data.visible} / ${data.total} · ${data.style_hint}`;
  const html = ['<table><thead><tr><th>工具</th><th>类别</th><th>需要</th><th>说明</th><th>可见</th></tr></thead><tbody>'];
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
    row.append(head, body);
    box.append(row);
  });
  box.scrollTop = box.scrollHeight;
}

async function refreshAgentHistory() {
  const { data } = await api("/api/agent/history?after=0");
  if (!data) return;
  state.agent.seq = data.seq || 0;
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
    ["temperature", data.temperature],
    ["timeout_s", (data.timeout_s || 0) + " s"],
    ["history", run.history || 0],
    ["pending", run.pending ? "queued" : "–"],
  ]);
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
  };
  const key = $("agent-api-key").value;
  if (key) body.api_key = key; // empty field = keep the stored key
  const { status, data } = await api("/api/agent/config", { method: "POST", body });
  toast(status === 200 ? t("saved") : (data && data.error) || t("failed"), status !== 200);
  if (status === 200) refreshAgentConfig().catch(() => {});
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
  if (!prompt) return toast("prompt 必填", true);
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
  if (!tool) return toast("tool 必填", true);
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
      prompt: $("rule-prompt").value.trim() || "看一下板子现在的状态",
      min_interval_s: Number($("rule-every").value) || 60,
      enabled: true,
    },
  }).then(() => {
    ["rule-id", "rule-tool", "rule-path", "rule-value", "rule-prompt", "rule-every"].forEach(
      (k) => ($(k).value = "")
    );
  });
}

async function refreshAgent() {
  await refreshAgentConfig().catch(() => {});
  await refreshAgentHistory().catch(() => {});
  await refreshSched().catch(() => {});
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
    if (!list.children.length) list.innerHTML = '<p class="hint">没有扫到网络。</p>';
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
  if (name === "agent") refreshAgent();
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
  $("lang-btn").addEventListener("click", (e) => {
    e.preventDefault();
    state.lang = state.lang === "zh" ? "en" : "zh";
    localStorage.setItem("lang", state.lang);
    applyLang();
    if (state.status) refreshStatus();
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
    if (!ssid) return toast("SSID 必填", true);
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
      return renderIssues([{ path: "(json)", message: "JSON 解析失败: " + err.message, hint: "" }]);
    }
    const { data } = await api("/api/manifest", { method: "POST", body: { manifest: doc, dry_run: true } });
    renderIssues((data && (data.issues || [])) || []);
    toast(data && data.applied === false ? "checked" : t("applied"));
  });
  $("manifest-apply").addEventListener("click", async () => {
    let doc;
    try { doc = JSON.parse($("manifest-text").value); } catch (err) {
      return renderIssues([{ path: "(json)", message: "JSON 解析失败: " + err.message, hint: "" }]);
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
  $("agent-config-save").addEventListener("click", saveAgentConfig);
  $("sched-refresh").addEventListener("click", refreshSched);
  $("sched-task-add").addEventListener("click", addTask);
  $("sched-rule-add").addEventListener("click", addRule);
}

(async function main() {
  applyLang();
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
    refreshAgentHistory().catch(() => {});
    refreshAgentConfig().catch(() => {});
    refreshSched().catch(() => {});
  }, 4000);
})();
