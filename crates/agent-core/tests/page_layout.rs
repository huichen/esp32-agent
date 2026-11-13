//! Rectangles, not strings: the composer has to fit the screen it is shown on.
//!
//! The bug this locks down was invisible to a source-level assertion, which is why the
//! assertion it replaces never caught it: `.attach-chip { display: flex }` outranked the
//! UA `[hidden] { display: none }`, so the chip stayed a 366 px flex item while it was
//! `hidden`, claimed the composer's single (nowrap) line, and pushed the field and the
//! send button past the right edge. Measured on a 390 px phone: the field began at x=428
//! and the send button at x=466. Every rule was written as intended; only the boxes were
//! wrong. So this test renders the shipped page in a headless browser, at the widths a
//! phone reports, and asserts on `getBoundingClientRect()`.
//!
//! The same rendered fixture exercises the chat clear control: confirmation, auth retry,
//! failure/busy preservation and successful clearing across delayed reads and SSE replay.
//!
//! It needs a Chromium-family browser. `ESP32_AGENT_BROWSER` points at one that is not in
//! a standard place (see `browser()`).

use esp32_agent_core::web::{APP_JS, INDEX_HTML, STYLE_CSS};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;

/// The widths a phone actually reports, in CSS pixels. 390 is an iPhone 14/15, 360 a
/// common Android. The composer is the bottom bar: it either fits inside these or the page
/// cannot be typed into.
const PHONE_WIDTHS: &[u32] = &[390, 360];
const PHONE_HEIGHT: u32 = 844;

/// Served into the shipped page after `app.js`. It measures three states — nothing
/// attached, an attachment chip shown, and the chip removed again — because each one puts
/// a different set of boxes in the composer.
///
/// The numbers come back through an attribute on the dumped DOM, percent-encoded so the
/// HTML serialisation cannot touch a single character of the JSON.
const MEASURE_JS: &str = r##"
<script>
window.__esp32Measure = function () {
  var q = function (s) { return document.querySelector(s); };
  var r = function (el) {
    var b = el.getBoundingClientRect();
    return { left: b.left, right: b.right, top: b.top, bottom: b.bottom, width: b.width, height: b.height };
  };
  var snap = function () { return { chip: r(q("#attach-chip")), input: r(q("#input")), send: r(q("#send")) }; };
  var attach = function (a) { S.attach = a; showAttach(); };

  // Markdown, measured as DOM: a table is a table, its cells kept their inline code and
  // their pipes, and it stays inside the bubble it was rendered into. A hidden host at the
  // page's own width, so the numbers are the ones a phone would produce.
  window.__esp32Markdown = function () {
    var host = document.createElement("div");
    host.style.cssText = "position:absolute;left:0;top:0;width:100%;visibility:hidden";
    var row = document.createElement("div");
    row.className = "row agent";
    var bubble = document.createElement("div");
    bubble.className = "bubble";
    row.appendChild(bubble);
    host.appendChild(row);
    document.body.appendChild(host);
    bubble.innerHTML = markdown([
      "检测到 4 个设备：",
      "",
      "| 设备 | 说明 | 引脚 | 可用工具 |",
      "|---|---|:---:|---|",
      "| buttons | 3 个按键 | GPIO 0、39、40 | `buttons_read`、`button_wait` |",
      "| led | WS2812 灯珠 | GPIO 48 | `led_set` |",
      "| a \\| b | 竖线 | — | `x` |",
      "",
      "结尾。",
    ].join("\n"));

    var table = bubble.querySelector("table");
    var wrap = bubble.querySelector(".table-wrap");
    var all = table ? Array.prototype.slice.call(table.querySelectorAll("th,td")) : [];
    var pipeOnly = document.createElement("div");
    pipeOnly.className = "bubble";
    pipeOnly.innerHTML = markdown("a | b\nc | d");
    var out = {
      table: !!table,
      inBubble: !!bubble.querySelector("table"),
      thead: table ? table.querySelectorAll("thead tr").length : 0,
      bodyRows: table ? table.querySelectorAll("tbody tr").length : 0,
      headers: table
        ? Array.prototype.map.call(table.querySelectorAll("th"), function (c) { return c.textContent; })
        : [],
      cells: all.map(function (c) { return c.textContent; }),
      centered: all.filter(function (c) { return c.style.textAlign === "center"; }).length,
      codeInCell: table ? table.querySelectorAll("td code").length : 0,
      pipeOnly: !!pipeOnly.querySelector("table"),
      text: bubble.textContent,
      wrap: wrap ? r(wrap) : null,
      bubble: r(bubble),
    };
    var links = document.createElement("div");
    links.innerHTML = markdown([
      '[safe HTTPS](https://example.test/a?q=1&b=2)',
      '[safe HTTP](http://example.test/)',
      '[safe mail](mailto:user@example.test)',
      '[safe relative](/api/files?path=/hello)',
      '[safe anchor](#memory)',
      '[bad js](javascript:alert(1))',
      '[bad case](JaVaScRiPt:alert(1))',
      '[bad tab](java\tscript:alert(1))',
      '[bad data](data:text/html,hello)',
      '[bad file](file:///etc/passwd)',
      '[bad vb](vbscript:msgbox(1))',
      '<img src=x onerror="alert(1)">',
      '[attribute text](https://example.test/&quot;)',
    ].join("\n\n"));
    out.links = Array.prototype.map.call(links.querySelectorAll("a"), function (a) {
      return { label: a.textContent, href: a.getAttribute("href"), protocol: a.protocol, onclick: a.getAttribute("onclick") };
    });
    out.rawImage = !!links.querySelector("img");
    host.remove();
    return out;
  };

  var out = { width: innerWidth, height: innerHeight };
  out.empty = snap();
  attach({ name: "IMG_20261112_1942.jpg", mime: "image/jpeg", data_b64: "", kb: 21 });
  out.attached = snap();
  attach(null);
  out.restored = snap();
  out.md = window.__esp32Markdown();
  // `pretty()` is what makes a tool argument or a result readable: a payload string with
  // its newlines escaped is the wall of text the console used to show. Three cases, because
  // leaving non-JSON alone matters as much as laying out JSON.
  out.prettyJson = pretty('{"a":1,"b":2}');
  out.prettyBlock = pretty('{"text":"line one\\nline two","bytes":3}');
  out.prettyPlain = pretty("not json at all");
  // Several private reads can get 401 together at startup. They must wait for the
  // same login, rather than overwrite the first unresolved callback.
  var firstUnlock = unlock();
  var secondUnlock = unlock();
  out.sharedUnlock = firstUnlock === secondUnlock;
  var finishUnlock = S.afterUnlock;
  S.afterUnlock = null;
  S.unlockPromise = null;
  q("#login").hidden = true;
  if (finishUnlock) finishUnlock(true);
  // A restored transcript must read like the live one: `renderTrace` lays out what came back
  // through the same `pretty()`, so a JSON result whose payload holds newlines is a block,
  // not one line with literal `\n` in it.
  q("#chat").innerHTML = "";
  renderTrace("", [{ kind: "call",
                    call: { id: "c1", name: "shell",
                            arguments: '{"command":"ps"}' },
                    result: { text: '{"ok":true,"stdout":"line one\\nline two"}', is_error: false } }]);
  out.tracePres = Array.prototype.map.call(q("#chat").querySelectorAll("details.tool pre"),
    function (p) { return p.textContent; });
  // The transcript's bottom padding must cover the composer whatever height it takes: a
  // one-line field, then three lines in the same field.
  out.chatPadBefore = parseFloat(getComputedStyle(q("#chat")).paddingBottom) || 0;
  out.composerHeightBefore = r(q("#composer")).height;
  var field = q("#input");
  field.value = "one\ntwo\nthree";
  autosize();
  out.chatPadAfter = parseFloat(getComputedStyle(q("#chat")).paddingBottom) || 0;
  out.composerHeightAfter = r(q("#composer")).height;
  field.value = "";
  autosize();
  return out;
};
</script>
"##;

/// Deterministic transport fixtures, but the actual shipped UI, event handlers and DOM.
/// The journal deliberately retains old events after reset, like a Memory forget can:
/// success must seed its head, not depend on a backend erasing the replay ring.
const CLEAR_JS: &str = r##"
<script>
window.__esp32Clear = async function () {
  const q = (s) => document.querySelector(s);
  const rect = (el) => {
    const b = el.getBoundingClientRect();
    return { left: b.left, right: b.right, top: b.top, bottom: b.bottom, width: b.width, height: b.height };
  };
  const defer = () => {
    let resolve;
    const promise = new Promise((r) => { resolve = r; });
    return { promise, resolve };
  };
  const tick = () => new Promise((r) => setTimeout(r, 0));
  // Answer the page's own confirmation whenever one is open. `f.confirmations` records what
  // it said, so a test can still check the wording, and `f.accept` decides the answer. Doing
  // it here means every waiter — not only `click` — makes progress instead of timing out with
  // a dialog on screen.
  const answerDialog = () => {
    const dialog = q("#confirm-dialog");
    if (!dialog || dialog.hidden) return false;
    f.confirmations.push(q("#confirm-title").textContent + "\n" + q("#confirm-body").textContent);
    if (!f.dialogButtons) f.dialogButtons = [q("#confirm-ok").textContent, q("#confirm-cancel").textContent];
    (f.accept ? q("#confirm-ok") : q("#confirm-cancel")).click();
    return true;
  };
  const until = async (test) => {
    for (let i = 0; i < 400; i++) {
      // Answering a dialog starts asynchronous work on the next microtask; checking the
      // predicate in the same turn would call an unfinished clear "settled".
      if (answerDialog()) { await tick(); continue; }
      if (test()) return;
      await tick();
    }
    throw new Error("fixture did not settle");
  };
  const click = async (button) => {
    button.click();
    await until(() => !S.changingConversation && !S.sending);
  };
  const old = [
    { role: "user", seq: 0, text: "OLD USER QUESTION" },
    { role: "assistant", seq: 1, text: "OLD INTERMEDIATE NOTE",
      tool_calls: [{ id: "old-call", name: "old_tool", arguments: '{"value":1}' }] },
    { role: "tool", seq: 2, text: "OLD TOOL RESULT", tool_call_id: "old-call" },
    { role: "assistant", seq: 3, text: "OLD ASSISTANT ANSWER" },
  ];
  const f = {
    messages: old.slice(), head: 80, requests: [], streams: [], confirmations: [], accept: true,
    resetMode: "success", resetGate: null, historyGate: null, messageGate: null, eventsFail: false,
    authRequired: false, issued: "fixture-" + Math.random(),
    journal: [
      { kind: "message_start", seq: 70 },
      { kind: "text_delta", seq: 71, text: "OLD REPLAY ANSWER" },
      { kind: "tool_use", seq: 72, name: "old_replay_tool", text: "{}" },
      { kind: "tool_result", seq: 73, text: "OLD REPLAY RESULT" },
      { kind: "run_end", seq: 80 },
    ],
  };
  const nativeFetch = window.fetch;
  const nativeSource = window.EventSource;
  const token = S.token;
  const storedToken = localStorage.getItem("esp32-agent.token");
  const response = (body, status = 200) => Promise.resolve(new Response(JSON.stringify(body), {
    status, headers: { "Content-Type": "application/json" },
  }));
  window.fetch = async (url, opts = {}) => {
    const path = new URL(url, location.href).pathname;
    if (!["/api/agent/history", "/api/agent/events", "/api/agent/reset", "/api/agent/forget",
          "/api/agent/message", "/api/auth", "/api/agent/config", "/api/agent/prompt",
          "/api/sched", "/api/devices"].includes(path)) return nativeFetch(url, opts);
    f.requests.push({ path, method: opts.method || "GET", body: opts.body || "",
      authorization: (opts.headers || {}).Authorization || "",
      contentType: (opts.headers || {})["Content-Type"] || "" });
    if (path === "/api/agent/history") {
      const messages = f.messages.slice();
      const gate = f.historyGate;
      if (gate) await gate.promise;
      return response({ messages, seq: messages.length });
    }
    if (path === "/api/agent/events") {
      if (f.eventsFail) return response({ error: "fixture cursor unavailable" }, 503);
      return response({ seq: f.head, events: f.journal });
    }
    if (path === "/api/auth") return response({ token: f.issued });
    if (path === "/api/agent/reset") {
      if (f.authRequired && (opts.headers || {}).Authorization !== "Bearer " + f.issued)
        return response({ error: "PIN required" }, 401);
      const gate = f.resetGate;
      if (gate) await gate.promise;
      if (f.resetMode === "busy") return response({ error: "agent run is active" }, 409);
      if (f.resetMode === "failed") return response({ error: "fixture reset failed" }, 500);
      const removed = f.messages.length;
      f.messages = [];
      return response({ reset: true, removed, kept: 0 });
    }
    if (path === "/api/agent/forget") {
      const body = JSON.parse(opts.body || "{}");
      const before = f.messages.length;
      let lastUser = -1;
      f.messages.forEach((m, i) => { if (m.role === "user") lastUser = i; });
      f.messages = body.keep && lastUser >= 0 ? f.messages.slice(lastUser) : [];
      return response({ ok: true, removed: before - f.messages.length, kept: f.messages.length });
    }
    if (path === "/api/agent/message") {
      const gate = f.messageGate;
      if (gate) await gate.promise;
      f.messages.push({ role: "user", seq: f.messages.length, text: JSON.parse(opts.body).text });
      return response({ ok: true });
    }
    if (path === "/api/agent/config") return response({ model: "fixture model", max_steps: 0,
      temperature: 0.2, tick_ms: 1000, run: { runs: 1 } });
    if (path === "/api/agent/prompt") return response({ history: f.messages.length, archive_lines: 0, bytes: 100 });
    if (path === "/api/sched") return response({ tasks: [] });
    return response({ devices: [], candidates: [], unidentified: [] });
  };
  window.EventSource = class extends EventTarget {
    constructor(url) {
      super();
      this.url = url;
      this.closed = false;
      f.streams.push(this);
      const after = Number(new URL(url, location.href).searchParams.get("after"));
      Promise.resolve().then(() => {
        if (this.closed) return;
        if (this.onopen) this.onopen();
        f.journal.filter((e) => e.seq > after).forEach((e) => this.emit(e.kind, e));
      });
    }
    // Still deliver after close to model already-queued browser callbacks.
    emit(kind, data) { this.dispatchEvent(new MessageEvent(kind, { data: JSON.stringify(data) })); }
    close() { this.closed = true; }
  };
  const posts = (path) => f.requests.filter((r) => r.method === "POST" && r.path === path);
  const clean = () => !/OLD |old_tool|old_replay_tool/.test(q("#chat").textContent);
  const out = {};
  try {
    stopStream(); S.busy = false; S.lastEvent = 80;
    await loadHistory();
    q("#title").textContent = "esp32-agent with a very long board name";
    q("#sub").textContent = "4 devices · 42 tools · a long model identifier";
    const clear = q("#clear-chat");
    clear.focus();
    out.header = { button: rect(clear), title: rect(q("#title")), menu: rect(q("#menu-btn")),
      sheetHidden: q("#sheet").hidden, label: clear.textContent.trim(), aria: clear.getAttribute("aria-label"),
      chineseLanguage: /[\u3000-\u9fff\uac00-\ud7af\uff00-\uffef]/.test(clear.textContent),
      focused: document.activeElement === clear,
      focusOutline: getComputedStyle(clear).outlineStyle, enabled: !clear.disabled };

    const beforeCancel = q("#chat").innerHTML;
    f.accept = false;
    await click(clear);
    out.cancel = { posts: posts("/api/agent/reset").length, confirmations: f.confirmations.slice(),
      dialogButtons: f.dialogButtons || [],
      preserved: q("#chat").innerHTML === beforeCancel, enabled: !clear.disabled };

    f.accept = true; f.resetMode = "failed";
    const beforeFailure = q("#chat").innerHTML;
    const firstRow = q("#chat").firstElementChild;
    await click(clear);
    out.failure = { posts: posts("/api/agent/reset").length, preserved: q("#chat").innerHTML === beforeFailure,
      sameRow: q("#chat").firstElementChild === firstRow, rendered: S.rendered, lastMsg: S.lastMsg,
      lastEvent: S.lastEvent, enabled: !clear.disabled, toast: q("#toast").textContent };

    // A read and a poll both captured the old history before reset; release them only
    // after the successful clear and its fresh history read have completed.
    f.historyGate = defer();
    const oldGate = f.historyGate;
    const staleRead = loadHistory();
    const stalePoll = pollHistory();
    f.historyGate = null;
    connectStream();
    const retired = S.stream;
    ensureLiveTrace(); liveCalls = 1;
    chip(liveTrace, "old live tool", [["result", "OLD LIVE RESULT"]]);
    ensureLiveBubble().textContent = "OLD LIVE ANSWER";
    showLiveNote();
    f.resetMode = "success"; f.authRequired = true; S.token = "";
    f.resetGate = defer();
    const beforeSuccess = posts("/api/agent/reset").length;
    clear.click();
    await until(() => !q("#login").hidden);
    const requestsDuring = f.requests.length;
    clear.click();
    q("#forget-all").onclick(); // also guarded if called outside the disabled button
    q("#input").value = "draft kept during clear";
    q("#input").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await pollHistory();
    out.pending = { disabled: clear.disabled && q("#forget-tail").disabled && q("#forget-all").disabled,
      sendDisabled: q("#send").disabled, ariaBusy: clear.getAttribute("aria-busy"),
      noOtherRequests: f.requests.length === requestsDuring, draft: q("#input").value };
    q("#login-go").click(); // fixture auth is local and does not need a real PIN
    await until(() => posts("/api/agent/reset").length === beforeSuccess + 2 && q("#login").hidden);
    f.resetGate.resolve();
    await until(() => !S.changingConversation);
    f.resetGate = null;
    oldGate.resolve();
    await Promise.all([staleRead, stalePoll]);
    retired.emit("text_delta", { seq: 900, text: "OLD QUEUED CALLBACK" });
    const successfulPost = posts("/api/agent/reset").slice(-1)[0];
    out.success = { clean: clean(), rows: q("#chat").querySelectorAll(".row").length,
      toolRows: q("#chat").querySelectorAll("details.tool").length, rendered: S.rendered, lastMsg: S.lastMsg,
      lastEvent: S.lastEvent, busy: S.busy, retiredClosed: retired.closed, streamStopped: S.stream === null,
      liveCleared: liveTrace === null && liveBubble === null && liveNote === null && liveCalls === 0,
      postsIncludingAuthRetry: posts("/api/agent/reset").length - beforeSuccess,
      authorized: successfulPost.authorization === "Bearer " + f.issued,
      body: successfulPost.body, contentType: successfulPost.contentType,
      draft: q("#input").value, enabled: !clear.disabled, toast: q("#toast").textContent };

    q("#input").value = "fresh question";
    q("#composer").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await until(() => !S.sending);
    const freshStream = S.stream;
    freshStream.emit("message_start", { seq: 81 });
    freshStream.emit("text_delta", { seq: 82, text: "fresh answer" });
    retired.emit("text_delta", { seq: 901, text: "OLD LATE ANSWER" });
    if (retired.onerror) retired.onerror();
    out.fresh = { after: new URL(freshStream.url, location.href).searchParams.get("after"),
      clean: clean(), text: q("#chat").textContent, activeStreamPreserved: S.stream === freshStream,
      lastEvent: S.lastEvent };
    f.messages.push({ role: "assistant", seq: 1, text: "fresh answer" }); f.head = 83;
    freshStream.emit("run_end", { seq: 83 });
    await until(() => S.rendered === 2);
    // An earlier run_end's delayed close must not close the next running turn.
    q("#input").value = "another fresh question";
    q("#composer").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await until(() => !S.sending);
    await new Promise((r) => setTimeout(r, 550));
    out.fresh.nextTurnStreamKept = S.stream === freshStream && !freshStream.closed && S.busy;
    S.busy = false; stopStream(); hideLiveNote();

    // A remote run wins the race after confirmation: 409 preserves both the old history
    // and the live turn, rather than pretending the clear cancelled that run.
    f.messages = old.slice(); f.head = 80; S.lastEvent = 80;
    await loadHistory(); connectStream();
    const busyStream = S.stream;
    f.resetMode = "busy"; f.resetGate = defer();
    clear.click();
    busyStream.emit("message_start", { seq: 81 });
    busyStream.emit("text_delta", { seq: 82, text: "still running" });
    const duringRun = q("#chat").innerHTML;
    f.resetGate.resolve();
    await until(() => !S.changingConversation);
    f.resetGate = null;
    out.busy = { preserved: q("#chat").innerHTML === duringRun,
      activeStreamPreserved: S.stream === busyStream && !busyStream.closed, busy: S.busy,
      liveNoteKept: !!liveNote, enabled: !clear.disabled, toast: q("#toast").textContent };
    const busyPosts = posts("/api/agent/reset").length;
    const busyConfirmations = f.confirmations.length;
    await click(clear);
    q("#forget-tail").onclick(); q("#forget-all").onclick();
    out.busy.localNoPost = posts("/api/agent/reset").length === busyPosts;
    out.busy.localNoConfirmation = f.confirmations.length === busyConfirmations;
    out.busy.forgetNoPost = posts("/api/agent/forget").length === 0;
    S.busy = false; stopStream(); hideLiveNote();

    f.resetMode = "success"; f.messageGate = defer();
    q("#input").value = "posting a turn";
    q("#composer").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    const beforeSendingClear = posts("/api/agent/reset").length;
    const beforeSendingConfirm = f.confirmations.length;
    clear.click();
    out.sending = { pending: S.sending, noClearPost: posts("/api/agent/reset").length === beforeSendingClear,
      noConfirmation: f.confirmations.length === beforeSendingConfirm };
    f.messageGate.resolve(); await until(() => !S.sending); f.messageGate = null;
    S.busy = false; stopStream();

    // A failed cursor refresh never re-enables replay from zero: retry the head before
    // sending, and leave the draft and empty conversation alone if that also fails.
    f.eventsFail = true;
    await click(clear);
    const beforeCursorSend = posts("/api/agent/message").length;
    q("#input").value = "retry after cursor failure";
    q("#composer").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await until(() => !S.sending);
    out.cursorFailure = { clean: clean(), lastEvent: S.lastEvent,
      noMessagePost: posts("/api/agent/message").length === beforeCursorSend,
      draft: q("#input").value, noStream: S.stream === null };
    f.eventsFail = false; f.head = 120;
    q("#composer").dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await until(() => !S.sending);
    out.cursorFailure.retryAfter = new URL(S.stream.url, location.href).searchParams.get("after");
    out.cursorFailure.retryPosted = posts("/api/agent/message").length === beforeCursorSend + 1;
    S.busy = false; stopStream();

    f.messages = [
      { role: "user", seq: 0, text: "OLD FIRST EXCHANGE" },
      { role: "assistant", seq: 1, text: "OLD FIRST ANSWER" },
      { role: "user", seq: 2, text: "last question" },
      { role: "assistant", seq: 3, text: "last answer" },
    ];
    f.head = 200;
    await loadHistory();
    q("#menu-btn").click(); q('[data-panel="agent"]').click();
    await click(q("#forget-tail"));
    await until(() => q("#conversation-kv").textContent.includes("Messages2"));
    out.memory = { tailBody: posts("/api/agent/forget")[0].body, tailClean: clean(),
      tailText: q("#chat").textContent, tailRendered: S.rendered, tailCursor: S.lastEvent,
      summaryRefreshed: q("#conversation-kv").textContent.includes("Messages2") };
    await click(q("#forget-all"));
    out.memory.allBody = posts("/api/agent/forget")[1].body;
    out.memory.allClean = clean(); out.memory.allRendered = S.rendered; out.memory.allCursor = S.lastEvent;
    out.memory.mutations = f.requests.filter((r) => r.method === "POST").map((r) => r.path);
    return out;
  } catch (e) {
    out.error = String(e.stack || e);
    return out;
  } finally {
    stopStream(); S.busy = false;
    window.fetch = nativeFetch; window.EventSource = nativeSource;
    S.token = token;
    if (storedToken === null) localStorage.removeItem("esp32-agent.token");
    else localStorage.setItem("esp32-agent.token", storedToken);
  }
};
</script>
"##;

/// The ports the headless browser will not connect to, so the harness never picks one.
///
/// The list is Chromium's `kRestrictedPorts` for the ranges an ephemeral port can land in;
/// the well-known ones below 1024 never come back from `bind(0)` and are left out.
fn chrome_allows_port(port: u16) -> bool {
    !matches!(port, 2049 | 3659 | 4045 | 5060 | 5061 | 6000 | 6566 | 6665..=6669 | 6697 | 10080)
}

/// The page under measurement, served so that `app.js` and the stylesheet are the shipped
/// files and the origin is a normal one (`localStorage`, `fetch` and the module-scope
/// helpers behind `showAttach` all behave as they do on the board).
fn page() -> String {
    INDEX_HTML.replace("</body>", &format!("{MEASURE_JS}{CLEAR_JS}</body>"))
}

/// The harness: one iframe per phone width, because an iframe *is* the viewport — that is
/// the only way to get a fixed-position composer measured at 390 px without depending on
/// how a browser treats `--window-size`. Measurement runs on `load`, after the page's own
/// `DOMContentLoaded` handler has wired the composer up.
fn harness() -> String {
    let widths = PHONE_WIDTHS.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>phone widths</title></head>
<body style="margin:0;background:#000">
<script>
const widths = [{widths}];
const frames = widths.map((w) => {{
  const f = document.createElement("iframe");
  f.style.cssText = "display:block;border:0;width:" + w + "px;height:{height}px";
  f.src = "/page";
  document.body.appendChild(f);
  return {{ w, f }};
}});
let measuring = false;
async function measure() {{
  if (measuring || frames.some(({{ f }}) => !f.contentWindow || !f.contentWindow.__esp32Measure)) return;
  measuring = true;
  const out = await Promise.all(frames.map(async ({{ w, f }}) => {{
    const metrics = f.contentWindow.__esp32Measure();
    metrics.clear = await f.contentWindow.__esp32Clear();
    return {{ requested: w, metrics }};
  }}));
  document.body.dataset.esp32Layout = encodeURIComponent(JSON.stringify(out));
}}
// Never run the async interaction fixture twice in the same iframe. The timer is only a
// fallback if `load` happened before the child scripts exposed their measurement hooks.
window.addEventListener("load", measure);
setTimeout(measure, 300);
</script>
</body></html>
"#,
        height = PHONE_HEIGHT,
    )
}

/// The three files the board would serve, on a loopback port. Startup API calls answer
/// 404, as on an unconfigured board; the interaction fixture supplies controlled responses
/// once measurement starts so failures and response ordering are reproducible.
fn serve() -> String {
    let routes: Vec<(String, String)> = vec![
        ("/harness".into(), harness()),
        ("/page".into(), page()),
        ("/assets/style.css".into(), STYLE_CSS.to_string()),
        ("/assets/app.js".into(), APP_JS.to_string()),
    ];
    let listener = loop {
        // Chrome refuses a list of ports outright (`ERR_UNSAFE_PORT`: 6000 for X11, 6665-6669
        // for IRC, 10080, ...). An ephemeral port lands on one now and then, which shows up as
        // "app.js did not run" in a test that has nothing to do with ports.
        let candidate = TcpListener::bind("127.0.0.1:0").expect("a loopback port for the harness");
        let port = candidate.local_addr().expect("the harness address").port();
        if chrome_allows_port(port) {
            break candidate;
        }
    };
    let address = listener.local_addr().expect("the harness address");
    let routes = Arc::new(routes);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let routes = Arc::clone(&routes);
            // One thread per request: the page asks for the stylesheet and the script at
            // the same time, and a single-threaded accept loop deadlocks a browser.
            thread::spawn(move || {
                let _ = answer(&mut stream, &routes);
            });
        }
    });
    format!("http://{address}")
}

fn answer(stream: &mut TcpStream, routes: &[(String, String)]) -> std::io::Result<()> {
    let mut buf = [0u8; 2048];
    let read = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_string();
    let body = routes.iter().find(|(route, _)| *route == path).map(|(_, body)| body);
    let (status, kind, body) = match body {
        Some(body) => ("200 OK", content_type(&path), body.as_str()),
        None => ("404 Not Found", "text/plain", "not found"),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else {
        "text/html"
    }
}

/// Chrome, Edge or Chromium. `ESP32_AGENT_BROWSER` wins, so a CI image can point at the one
/// it installed; otherwise the standard locations, then `PATH`.
fn browser() -> PathBuf {
    if let Some(exe) = std::env::var_os("ESP32_AGENT_BROWSER") {
        return PathBuf::from(exe);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(dir) = std::env::var_os(base) {
                let dir = PathBuf::from(dir);
                candidates.push(dir.join("Google/Chrome/Application/chrome.exe"));
                candidates.push(dir.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
    } else if cfg!(target_os = "macos") {
        candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        candidates.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
        candidates.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
    } else {
        candidates.extend(
            ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge"]
                .into_iter()
                .filter_map(in_path),
        );
    }
    candidates.into_iter().find(|path| path.exists()).unwrap_or_else(|| {
        panic!(
            "this test measures the page in a browser and found none; install Chrome/Chromium \
             or set ESP32_AGENT_BROWSER to the executable"
        )
    })
}

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|candidate| candidate.exists())
}

/// Render the harness and hand back the dumped DOM. A unique profile per run matters: with
/// the default one Chrome hands the request to a browser that is already running and prints
/// nothing at all. Per *run*, not per process: the tests in this file run in parallel
/// threads of one process, and two browsers handed the same `--user-data-dir` do the same
/// thing — the second prints nothing (exit code 21).
fn dump_dom(browser: &Path, url: &str) -> String {
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let profile = std::env::temp_dir().join(format!("esp32-agent-layout-{}-{run}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let out = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--disable-sync",
            // A phone-sized iframe inside a wide window: the window size is irrelevant.
            "--window-size=1200,1000",
            "--virtual-time-budget=3000",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--dump-dom")
        .arg(url)
        .output()
        .unwrap_or_else(|e| panic!("could not run {}: {e}", browser.display()));
    let _ = std::fs::remove_dir_all(&profile);
    let dom = String::from_utf8_lossy(&out.stdout).into_owned();
    if !dom.contains("esp32Layout") {
        panic!(
            "{} printed no measurement (status {}): {}\nstderr: {}",
            browser.display(),
            out.status,
            url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    dom
}

/// `data-esp32-layout="<percent-encoded JSON>"` → one measurement per requested width.
fn measurements(dom: &str) -> BTreeMap<u32, Value> {
    let marker = "data-esp32-layout=\"";
    let start = dom.find(marker).unwrap_or_else(|| panic!("the harness left no measurement in:\n{dom}"))
        + marker.len();
    let raw = &dom[start..];
    let encoded = &raw[..raw.find('"').unwrap_or_else(|| panic!("unterminated measurement attribute"))];
    let json = percent_decode(encoded);
    let rows: Vec<Value> =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("the measurement is not JSON ({e}): {json}"));
    rows.into_iter()
        .map(|row| {
            let width = row["requested"].as_u64().unwrap_or(0) as u32;
            let metrics = row["metrics"].clone();
            assert!(
                !metrics.is_null(),
                "the page did not expose __esp32Measure at {width} px: app.js did not run"
            );
            (width, metrics)
        })
        .collect()
}

fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    width: f64,
    height: f64,
}

fn rect(metrics: &Value, state: &str, part: &str) -> Rect {
    let at = &metrics[state][part];
    let num = |key: &str| at[key].as_f64().unwrap_or(f64::NAN);
    Rect {
        left: num("left"),
        right: num("right"),
        top: num("top"),
        bottom: num("bottom"),
        width: num("width"),
        height: num("height"),
    }
}

/// Frontend regressions are asserted against the shipped controls and actual DOM, not
/// just a helper's return value. Keep these in the existing layout test (no test-count drift).
fn assert_conversation_clear(metrics: &Value, width: u32) {
    let clear = &metrics["clear"];
    assert!(clear["error"].is_null(), "{width} px: clear fixture failed: {clear}");
    let expect = |section: &str, key: &str, value: Value| {
        assert_eq!(clear[section][key], value, "{width} px: clear {section}.{key}: {clear}");
    };
    let text = |section: &str, key: &str| clear[section][key].as_str().unwrap_or("");
    let button = rect(clear, "header", "button");
    let title = rect(clear, "header", "title");
    let menu = rect(clear, "header", "menu");
    assert!(
        button.width >= 44.0 && button.height >= 44.0,
        "{width} px: clear needs a touch target, not {button:?}"
    );
    assert!(
        button.left >= 0.0 && button.right <= width as f64 + 0.01 && button.top >= 0.0,
        "{width} px: clear is outside the viewport: {button:?}"
    );
    assert!(
        menu.right <= title.left && title.right <= button.left,
        "{width} px: header controls overlap: menu {menu:?}, title {title:?}, clear {button:?}"
    );
    for key in ["sheetHidden", "focused", "enabled"] {
        expect("header", key, json!(true));
    }
    expect("header", "aria", json!("Clear conversation"));
    expect("header", "chineseLanguage", json!(false));
    assert!(text("header", "label").contains("Clear chat"), "the clear action must be named");
    assert_ne!(text("header", "focusOutline"), "none", "{width} px: invisible keyboard focus");

    expect("cancel", "posts", json!(0));
    expect("cancel", "preserved", json!(true));
    expect("cancel", "enabled", json!(true));
    // The dialog's own words, not the browser's: these are the buttons the device's
    // language used to draw, and they are ours now.
    expect("cancel", "dialogButtons", json!(["Clear", "Cancel"]));
    let confirmations = clear["cancel"]["confirmations"].as_array().expect("confirmation fixture");
    assert_eq!(confirmations.len(), 1, "{width} px: confirmation was not offered exactly once");
    let confirmation = confirmations[0].as_str().unwrap_or("");
    assert!(confirmation.contains("Clear conversation"), "the dialog names the action: {confirmation}");
    assert!(
        confirmation.contains("Instructions, skills, files and model settings stay."),
        "{width} px: confirmation must state the boundary: {confirmation}"
    );
    assert!(
        !confirmation.chars().any(|c| ('\u{3000}'..='\u{9fff}').contains(&c)),
        "{width} px: the dialog must be English only: {confirmation}"
    );

    expect("failure", "posts", json!(1));
    for key in ["preserved", "sameRow", "enabled"] {
        expect("failure", key, json!(true));
    }
    expect("failure", "rendered", json!(4));
    expect("failure", "lastMsg", json!(3));
    expect("failure", "lastEvent", json!(80));
    assert!(text("failure", "toast").contains("History was kept."));
    assert!(text("failure", "toast").contains("fixture reset failed"));

    for key in ["disabled", "sendDisabled", "noOtherRequests"] {
        expect("pending", key, json!(true));
    }
    expect("pending", "ariaBusy", json!("true"));
    expect("pending", "draft", json!("draft kept during clear"));
    for key in ["clean", "retiredClosed", "streamStopped", "liveCleared", "authorized", "enabled"] {
        expect("success", key, json!(true));
    }
    expect("success", "rows", json!(1));
    expect("success", "toolRows", json!(0));
    expect("success", "rendered", json!(0));
    expect("success", "lastMsg", json!(0));
    expect("success", "lastEvent", json!(80));
    expect("success", "busy", json!(false));
    expect("success", "postsIncludingAuthRetry", json!(2));
    expect("success", "body", json!("{}"));
    expect("success", "contentType", json!("application/json"));
    expect("success", "draft", json!("draft kept during clear"));
    assert!(text("success", "toast").contains("Conversation cleared."));

    expect("fresh", "after", json!("80"));
    expect("fresh", "lastEvent", json!(82));
    for key in ["clean", "activeStreamPreserved", "nextTurnStreamKept"] {
        expect("fresh", key, json!(true));
    }
    assert!(
        text("fresh", "text").contains("fresh question") && text("fresh", "text").contains("fresh answer")
    );
    for key in [
        "preserved",
        "activeStreamPreserved",
        "busy",
        "liveNoteKept",
        "enabled",
        "localNoPost",
        "localNoConfirmation",
        "forgetNoPost",
    ] {
        expect("busy", key, json!(true));
    }
    assert!(text("busy", "toast").contains("Wait for this turn to finish"));
    assert!(text("busy", "toast").contains("clearing does not cancel it"));
    for key in ["pending", "noClearPost", "noConfirmation"] {
        expect("sending", key, json!(true));
    }
    for key in ["clean", "noMessagePost", "noStream", "retryPosted"] {
        expect("cursorFailure", key, json!(true));
    }
    expect("cursorFailure", "lastEvent", Value::Null);
    expect("cursorFailure", "draft", json!("retry after cursor failure"));
    expect("cursorFailure", "retryAfter", json!("120"));

    expect("memory", "tailBody", json!("{\"keep\":1}"));
    expect("memory", "tailClean", json!(true));
    expect("memory", "tailRendered", json!(2));
    expect("memory", "tailCursor", json!(200));
    expect("memory", "summaryRefreshed", json!(true));
    assert!(
        text("memory", "tailText").contains("last question")
            && text("memory", "tailText").contains("last answer")
    );
    expect("memory", "allBody", json!("{}"));
    expect("memory", "allClean", json!(true));
    expect("memory", "allRendered", json!(0));
    expect("memory", "allCursor", json!(200));
    let mutations = clear["memory"]["mutations"].as_array().expect("recorded POST requests");
    assert!(
        mutations.iter().all(|path| [
            "/api/auth",
            "/api/agent/reset",
            "/api/agent/forget",
            "/api/agent/message",
        ]
        .contains(&path.as_str().unwrap_or(""))),
        "{width} px: clearing wrote to settings or the workspace: {mutations:?}"
    );
}

#[test]
fn the_composer_fits_a_phone_screen() {
    let browser = browser();
    let url = format!("{}/harness", serve());
    let dom = dump_dom(&browser, &url);
    let measured = measurements(&dom);

    for width in PHONE_WIDTHS {
        let viewport = *width as f64;
        let metrics = measured.get(width).unwrap_or_else(|| {
            panic!("no measurement for a {width} px viewport; the harness returned {measured:?}")
        });
        let reported = metrics["width"].as_f64().unwrap_or(f64::NAN);
        assert!(
            (reported - viewport).abs() < 1.0,
            "the page measured {reported} px wide, not {width}: the harness did not get a phone viewport"
        );

        // A failure should not send anyone back to the bench for the numbers: the whole
        // measurement is printed, and libtest shows it when the assertion trips.
        println!("{width} px viewport (the page reports {reported} px)");
        for state in ["empty", "attached", "restored"] {
            let c = rect(metrics, state, "chip");
            let i = rect(metrics, state, "input");
            let s = rect(metrics, state, "send");
            println!(
                "  {state:8} chip {:6.1}..{:6.1} top {:6.1} w {:5.1} h {:4.1} | \
                 field {:6.1}..{:6.1} w {:5.1} | send {:6.1}..{:6.1}",
                c.left, c.right, c.top, c.width, c.height, i.left, i.right, i.width, s.left, s.right
            );
        }

        for state in ["empty", "attached", "restored"] {
            let chip = rect(metrics, state, "chip");
            let input = rect(metrics, state, "input");
            let send = rect(metrics, state, "send");

            if state == "attached" {
                // The chip is the reason the composer wraps: on its own line, above the
                // field, inside the screen. Without `flex-wrap` it shares the one line and
                // pushes everything out again.
                assert!(
                    chip.width > 0.0 && chip.height > 0.0,
                    "{width} px, chip attached: the chip has no box ({chip:?}), so this state did not happen"
                );
                assert!(
                    chip.left >= 0.0 && chip.right <= viewport + 0.01,
                    "{width} px, chip attached: the chip spans {:.1}..{:.1}, outside the screen",
                    chip.left,
                    chip.right
                );
                assert!(
                    chip.bottom <= input.top + 0.5,
                    "{width} px, chip attached: the chip ends at y={:.1} and the field starts at y={:.1} — \
                     they are on one line, so the composer is not wrapping",
                    chip.bottom,
                    input.top
                );
            } else {
                // `hidden` has to mean no box at all. A hidden element that keeps its
                // `display` keeps its size, and a 366 px one is what shoved the field out.
                assert!(
                    chip.width == 0.0 && chip.height == 0.0,
                    "{width} px, chip {state}: a hidden chip still occupies {:.1}x{:.1} at x={:.1}",
                    chip.width,
                    chip.height,
                    chip.left
                );
            }

            assert!(
                input.left >= 0.0 && input.right <= viewport + 0.01,
                "{width} px, chip {state}: the field spans {:.1}..{:.1}, outside the screen",
                input.left,
                input.right
            );
            assert!(
                input.width >= 150.0,
                "{width} px, chip {state}: the field is only {:.1} px wide — not a field to type into",
                input.width
            );
            assert!(
                send.left >= 0.0 && send.right <= viewport + 0.01,
                "{width} px, chip {state}: the send button spans {:.1}..{:.1}, outside the screen",
                send.left,
                send.right
            );
            assert!(
                send.width >= 30.0,
                "{width} px, chip {state}: the send button is only {:.1} px wide",
                send.width
            );
        }
        assert_conversation_clear(metrics, *width);
    }
}

/// A table is a table: the model answers a device question with four columns, and on a
/// phone the unrendered version is one paragraph of `|` and `---`. Measured as DOM, not as
/// a string — the cells, the inline code inside them, an escaped pipe, the centred column,
/// and the part a string cannot say: that the table stays inside its bubble.
#[test]
fn a_markdown_table_renders_as_a_table_on_a_phone() {
    let browser = browser();
    let url = format!("{}/harness", serve());
    let dom = dump_dom(&browser, &url);
    let measured = measurements(&dom);

    for width in PHONE_WIDTHS {
        let md = &measured[width]["md"];
        let at = format!("{width} px");
        assert!(md["table"].as_bool().unwrap_or(false), "{at}: no <table> rendered: {md}");
        assert!(md["inBubble"].as_bool().unwrap_or(false), "{at}: the table left its bubble: {md}");
        assert_eq!(md["thead"], json!(1), "{at}: one header row: {md}");
        assert_eq!(md["bodyRows"], json!(3), "{at}: three body rows: {md}");
        assert_eq!(
            md["headers"],
            json!(["设备", "说明", "引脚", "可用工具"]),
            "{at}: the header cells: {md}"
        );
        assert!(
            md["codeInCell"].as_u64().unwrap_or(0) >= 3,
            "{at}: `code` inside a cell must still be code: {md}"
        );
        assert!(
            md["centered"].as_u64().unwrap_or(0) >= 4,
            "{at}: `:---:` is one centred column over four rendered rows: {md}"
        );
        assert!(
            md["cells"].as_array().into_iter().flatten().any(|c| c == "a | b"),
            "{at}: `\\|` is a pipe in the text, not a separator: {md}"
        );
        let links = md["links"].as_array().expect("the rendered link fixtures");
        assert_eq!(links.len(), 6, "{at}: active-content URLs became links: {links:?}");
        assert!(
            links.iter().all(|link| {
                ["http:", "https:", "mailto:"].contains(&link["protocol"].as_str().unwrap_or(""))
                    && link["onclick"].is_null()
                    && !link["label"].as_str().unwrap_or("").starts_with("bad")
            }),
            "{at}: unsafe Markdown links: {links:?}"
        );
        assert_eq!(links[0]["href"], "https://example.test/a?q=1&b=2", "queries stay intact");
        assert_eq!(md["rawImage"], false, "raw HTML must stay text");
        assert_eq!(
            measured[width]["sharedUnlock"], true,
            "{at}: concurrent PIN reads stranded a login waiter"
        );
        assert_eq!(
            md["pipeOnly"],
            json!(false),
            "{at}: pipes without a `|---|` row are text, not a table: {md}"
        );
        assert!(
            !md["text"].as_str().unwrap_or("").contains("|---"),
            "{at}: the delimiter row leaked into the text: {md}"
        );

        // A tool argument or result is laid out, not printed as one line with `\n` in it:
        // the payload string becomes a block, the rest of the object trails it as one line.
        let pretty_json = measured[width]["prettyJson"].as_str().unwrap_or("");
        assert!(
            pretty_json.contains("\"a\": 1") && pretty_json.contains('\n'),
            "{at}: JSON was not indented: {pretty_json:?}"
        );
        let pretty_block = measured[width]["prettyBlock"].as_str().unwrap_or("");
        assert!(
            pretty_block.contains("line one\nline two"),
            "{at}: the newline inside the payload stayed escaped: {pretty_block:?}"
        );
        assert!(
            pretty_block.contains("\u{2014} bytes: 3"),
            "{at}: the other fields were dropped: {pretty_block:?}"
        );
        assert_eq!(
            measured[width]["prettyPlain"].as_str().unwrap_or(""),
            "not json at all",
            "{at}: text that is not JSON must be left alone"
        );

        // A restored transcript goes through the same formatter as the live chips; before
        // this the result of a tool call came back as one line of JSON with `\n` in it.
        let pres: Vec<String> = measured[width]["tracePres"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        assert_eq!(pres.len(), 2, "{at}: a call and its result, not {pres:?}");
        assert!(
            pres[0].starts_with("arguments: ") && pres[0].contains("\"command\": \"ps\""),
            "{at}: the arguments were not laid out: {pres:?}"
        );
        assert!(
            pres[1].contains("line one\nline two"),
            "{at}: the restored result kept its newline escaped: {pres:?}"
        );
        assert!(!pres[1].contains("\\n"), "{at}: the restored result still carries a literal \\n: {pres:?}");

        // The transcript must not run under the composer: its padding covers whatever the
        // composer currently is, not the 84 px a one-line field used to need.
        let pad_before = measured[width]["chatPadBefore"].as_f64().unwrap_or(0.0);
        let pad_after = measured[width]["chatPadAfter"].as_f64().unwrap_or(0.0);
        let h_before = measured[width]["composerHeightBefore"].as_f64().unwrap_or(0.0);
        let h_after = measured[width]["composerHeightAfter"].as_f64().unwrap_or(0.0);
        assert!(
            pad_before >= h_before + 9.0,
            "{width} px: {pad_before} px of padding for a {h_before} px composer"
        );
        assert!(
            h_after > h_before + 20.0,
            "{width} px: the three-line field did not grow the composer ({h_before} -> {h_after})"
        );
        assert!(
            pad_after >= h_after + 9.0,
            "{width} px: a {h_after} px composer but only {pad_after} px of padding"
        );
        assert!(
            pad_after > pad_before,
            "{width} px: padding did not follow the field ({pad_before} -> {pad_after})"
        );

        // A wide table may scroll inside its wrapper; what it may not do is make the bubble
        // wider than the row it sits in.
        let wrap = &md["wrap"];
        let bubble = &md["bubble"];
        let num = |v: &Value, key: &str| v[key].as_f64().unwrap_or(f64::NAN);
        assert!(
            num(wrap, "left") >= num(bubble, "left") - 0.5
                && num(wrap, "right") <= num(bubble, "right") + 0.5,
            "{at}: the table spans {:.1}..{:.1}, the bubble {:.1}..{:.1}",
            num(wrap, "left"),
            num(wrap, "right"),
            num(bubble, "left"),
            num(bubble, "right")
        );
        assert!(
            num(bubble, "width") <= *width as f64 * 0.9,
            "{at}: the bubble is {:.1} px wide, more than the 86% a bubble may take",
            num(bubble, "width")
        );
    }
}
