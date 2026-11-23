// kmplify-node local dashboard. Everything it shows comes from /api/state on
// this computer; every value is written with textContent, never as markup.
"use strict";

const TOKEN = document.querySelector('meta[name="kn-token"]').content;
const OS_NAMES = { windows: "Windows", macos: "macOS", linux: "Linux" };
const LEVEL_TEXT = { trusted: "Trusted", cloud: "Cloud", community: "Community", unknown: "Not known yet" };
let state = null;
let current = "overview";
let logFilter = "";
let editing = false;

function el(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) {
    if (kid === null || kid === undefined || kid === false) continue;
    e.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return e;
}

function card(title, ...kids) {
  return el("div", { class: "card" }, title ? el("h2", { text: title }) : null, ...kids);
}

function kv(pairs) {
  const dl = el("dl", { class: "kv" });
  for (const [k, v] of pairs) {
    if (v === null || v === undefined || v === "") continue;
    dl.append(el("dt", { text: k }), el("dd", {}, v));
  }
  return dl;
}

function dur(s) {
  if (s === null || s === undefined) return "";
  s = Math.max(0, Math.floor(s));
  if (s < 60) return s + " s";
  if (s < 3600) return Math.floor(s / 60) + " min";
  if (s < 86400) return Math.floor(s / 3600) + " h " + Math.floor((s % 3600) / 60) + " min";
  return Math.floor(s / 86400) + " d " + Math.floor((s % 86400) / 3600) + " h";
}

function ago(unixS) {
  if (!unixS) return "never";
  return dur(Date.now() / 1000 - unixS) + " ago";
}

function gb(mb) {
  if (mb === null || mb === undefined) return "";
  if (mb === 0) return "0 GB";
  return (mb / 1024).toFixed(mb >= 10240 ? 0 : 1) + " GB";
}

function onoff(v) {
  return el("span", { class: "chip " + (v ? "ok" : ""), text: v ? "on" : "off" });
}

function meter(pct) {
  const p = Math.max(0, Math.min(100, pct || 0));
  const bar = el("span");
  bar.style.width = p + "%";
  return el("div", { class: "meter", role: "img", "aria-label": Math.round(p) + " percent" }, bar);
}

function toast(msg) {
  const t = el("div", { class: "toast", role: "status", text: msg });
  document.body.append(t);
  setTimeout(() => t.remove(), 4000);
}

async function api(path, body) {
  const opts = { headers: { "x-kmplify-node-token": TOKEN }, cache: "no-store" };
  if (body !== undefined) {
    opts.method = "POST";
    opts.headers["content-type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const r = await fetch(path, opts);
  let data = null;
  try { data = await r.json(); } catch (_) { data = null; }
  if (!r.ok && body === undefined) throw new Error("http " + r.status);
  return data || {};
}

async function act(path, body) {
  try {
    const r = await api(path, body || {});
    toast(r.message || "done");
  } catch (e) {
    toast("The node did not answer: " + e.message);
  }
  refresh();
}

// ----------------------------------------------------------------- header

function header(s) {
  const n = s.node;
  document.getElementById("sub").textContent =
    "v" + s.version + (n.id_short ? " | node " + n.id_short : "") + " | " + (OS_NAMES[s.os] || s.os);
  const link = document.getElementById("pill-link");
  const online = n.link === "ONLINE";
  link.className = "pill " + (online ? (n.paused ? "warn" : "ok") : "warn");
  link.textContent = online ? (n.paused ? "Online, paused" : "Online") : n.link.charAt(0) + n.link.slice(1).toLowerCase();
  const st = s.trust.standing;
  const trust = document.getElementById("pill-trust");
  trust.className = "pill " + (st.level === "trusted" || st.level === "cloud" ? "ok" : st.level === "community" ? "warn" : "");
  trust.textContent = LEVEL_TEXT[st.level] || st.level;
  const badge = document.getElementById("badge-trust");
  const bad = s.trust.checklist.filter((i) => i.state === "fail" || i.state === "unknown").length;
  badge.hidden = bad === 0;
  badge.textContent = String(bad);
}

// --------------------------------------------------------------- overview

function overview(s) {
  const n = s.node, m = s.machine, sh = s.sharing, st = s.trust.standing;
  const conn = card("Connection",
    kv([
      ["Fabric", n.gateway],
      ["State", n.link === "ONLINE" ? "connected" + (n.connected_s !== null ? " for " + dur(n.connected_s) : "") : n.link.toLowerCase()],
      ["Last problem", n.link !== "ONLINE" ? n.link_detail : ""],
      ["Node", n.id_short ? n.id_short + "..." : "no identity yet"],
      ["Version", s.version],
      ["Running for", dur(n.uptime_s)],
      ["Reconnects", String(n.reconnects)],
    ]),
    el("div", { class: "actions" },
      n.paused
        ? el("button", { class: "btn", onclick: () => act("/api/resume") }, "Resume sharing")
        : el("button", { class: "btn ghost", onclick: () => act("/api/pause") }, "Pause sharing"),
      el("button", { class: "btn ghost", onclick: () => act("/api/reconnect") }, "Reconnect now")),
    n.paused ? el("p", { class: "muted small", text: "Paused: the connection and running sessions stay up; nothing new is offered until you resume." }) : null);

  const models = sh.inference && sh.models.length === 0;
  const shares = card("What this node shares",
    el("div", { class: "row" }, el("div", { class: "grow" }, el("strong", { text: "Chat models" }),
      el("div", { class: "small " + (models ? "" : "muted"), text: sh.models_text })),
      sh.inference ? el("span", { class: "chip " + (sh.models.length ? "ok" : "warn"), text: sh.models.length ? String(sh.models.length) : "none" }) : onoff(false)),
    el("div", { class: "row" }, el("div", { class: "grow" }, el("strong", { text: "Serverless apps" }),
      el("div", { class: "small muted", text: sh.workloads.length ? sh.workloads.join(", ") + " (each runs as a container session)" : "No app templates offered." })),
      onoff(sh.workloads.length > 0)),
    el("div", { class: "row" }, el("div", { class: "grow" }, el("strong", { text: "CPU and RAM" }),
      el("div", { class: "small muted", text: sh.max_cpus ? "up to " + sh.max_cpus + " threads" : "lend spare threads and memory" })), onoff(sh.cpu)),
    el("div", { class: "row" }, el("div", { class: "grow" }, el("strong", { text: "Functions" }),
      el("div", { class: "small muted", text: "signed Wasm functions in a sandbox" })), onoff(sh.functions)),
    el("div", { class: "row" }, el("div", { class: "grow" }, el("strong", { text: "Vectors" }),
      el("div", { class: "small muted", text: "replicated vector collections" })), onoff(sh.vectors)),
    el("p", { class: "muted small" }, "Engine: ", el("code", { text: sh.engine || "not set" }),
      sh.approval_mode ? " | admission: " + sh.approval_mode : "", sh.country ? " | country: " + sh.country : ""));

  const machine = card("This machine",
    kv([
      ["GPU", m.gpu_name ? m.gpu_name + (m.vram_total_mb ? ", " + gb(m.vram_total_mb) : "") + " (" + m.accelerator + ")" : (m.accelerator || "none detected")],
      ["CPU", (m.cpu_model ? m.cpu_model + ", " : "") + (m.cpus ? m.cpus + " threads" : "")],
      ["RAM", m.ram_total_mb ? gb(m.ram_used_mb) + " of " + gb(m.ram_total_mb) + " in use" : ""],
      ["Session disk", m.fabric_disk_mb !== null ? gb(m.fabric_disk_mb) + " used by sessions" : ""],
    ]),
    el("h3", { text: "CPU " + Math.round(m.cpu_percent || 0) + "%" }), meter(m.cpu_percent),
    m.ram_total_mb ? [el("h3", { text: "RAM " + Math.round(100 * m.ram_used_mb / m.ram_total_mb) + "%" }), meter(100 * m.ram_used_mb / m.ram_total_mb)] : null,
    m.gpu_percent !== null && m.gpu_percent !== undefined ? [el("h3", { text: "GPU " + m.gpu_percent + "%" }), meter(m.gpu_percent)] : el("p", { class: "muted small", text: "GPU load: this platform does not report it." }));

  const missing = st.missing.slice(0, 4);
  const trust = card("Trusted node",
    el("p", { class: "lead" }, el("span", { class: "pill " + (st.level === "trusted" || st.level === "cloud" ? "ok" : st.level === "community" ? "warn" : ""), text: LEVEL_TEXT[st.level] || st.level }), " ", st.headline),
    missing.length ? el("ul", {}, missing.map((m) => el("li", { text: m }))) : el("p", { class: "muted", text: "Nothing is missing." }),
    st.missing.length > missing.length ? el("p", { class: "muted small", text: "and " + (st.missing.length - missing.length) + " more" }) : null,
    el("div", { class: "actions" }, el("button", { class: "btn", onclick: () => show("trust") }, "Open the checklist")));

  return el("div", { class: "grid" }, trust, shares, conn, machine);
}

// ------------------------------------------------------------------ trust

function trustTab(s) {
  const t = s.trust, st = t.standing, f = t.fabric;
  const level = LEVEL_TEXT[st.level] || st.level;
  const head = card("Where this node stands",
    el("p", { class: "lead" }, el("span", { class: "pill " + (st.level === "trusted" || st.level === "cloud" ? "ok" : st.level === "community" ? "warn" : ""), text: level }), " ", st.headline),
    kv([
      ["The fabric says", t.fabric_line || "not read yet"],
      ["Site", f && f.site ? f.site : ""],
      ["This machine", st.baseline_ok === null || st.baseline_ok === undefined ? "check still running" : st.baseline_ok ? "meets the baseline" : "does not meet the baseline yet"],
      ["Last check", t.checked_at ? ago(t.checked_at) + " (checker " + t.checker_version + ")" : "running"],
      ["Administrator rights", t.elevated === null || t.elevated === undefined ? "" : t.elevated ? "yes" : "no"],
    ]),
    el("div", { class: "actions" },
      el("button", { class: "btn", disabled: t.rechecking, onclick: () => act("/api/recheck") }, t.rechecking ? "Checking..." : "Re-check now")),
    el("p", { class: "muted small", text: "Re-check runs the self-check again and sends the signed result to the fabric with the next heartbeat. The node checks itself every 3 hours anyway; a result older than 6 hours no longer counts." }));

  const missing = card("Missing for a Trusted node",
    st.missing.length
      ? el("ul", {}, st.missing.map((m) => el("li", { text: m })))
      : el("p", { class: "muted", text: "Nothing is missing." }));
  const what = card("What Trusted means",
    el("p", { class: "small", text: "Personal or confidential work runs only on Trusted nodes. Trusted needs both:" }),
    el("ol", { class: "small" },
      el("li", { text: "Your KMPLIFY provider account passed the identity check (trader verified, 2FA on), and KMPLIFY granted Trusted for this node." }),
      el("li", { text: "This machine meets the security baseline below. The node checks it itself and the fabric evaluates the signed result." })),
    el("p", { class: "muted small", text: "The fabric decides the level; the node never sets it. What the node cannot read counts as failing." }));

  const list = el("div", {});
  if (!t.checklist.length) {
    list.append(card(null, el("p", { text: "The security check is still running. On Windows the update search can take up to two minutes." })));
  }
  for (const i of t.checklist) {
    const bad = i.state === "fail" || i.state === "unknown";
    const sym = { pass: "✓", fail: "✕", unknown: "?", not_applicable: "–" }[i.state];
    const word = { pass: "passes", fail: "fails", unknown: "unknown", not_applicable: "not checked" }[i.state];
    const fab = i.fabric === null || i.fabric === undefined
      ? el("span", { class: "chip", text: "fabric: not known" })
      : el("span", { class: "chip " + (i.fabric === "pass" ? "ok" : "bad"), text: "fabric: " + (i.fabric === "pass" ? "passes" : "fails") });
    const others = i.other_os.map(([os, steps]) =>
      el("div", { class: "small" }, el("strong", { text: OS_NAMES[os] || os }), el("ol", {}, steps.map((x) => el("li", { text: x })))));
    list.append(el("div", { class: "item" + (bad ? " bad" : "") },
      el("div", { class: "item-head" },
        el("span", { class: "mark " + i.state, "aria-hidden": "true", text: sym }),
        el("span", { class: "name", text: i.label }),
        el("span", { class: "chips" }, el("span", { class: "chip " + (bad ? "bad" : i.state === "pass" ? "ok" : ""), text: word }), fab)),
      el("div", { class: "small", text: i.text }),
      i.detail ? el("div", { class: "small muted", text: i.detail }) : null,
      i.steps.length ? [el("h3", { text: "How to fix it on " + (OS_NAMES[s.os] || s.os) }), el("ol", {}, i.steps.map((x) => el("li", { text: x })))] : null,
      el("details", {}, el("summary", { text: "Steps on other systems" }), others)));
  }
  const notes = t.hints.length
    ? card("Notes from the check", el("ul", {}, t.hints.map((h) => el("li", { class: "small", text: h }))))
    : null;
  return el("div", {}, el("div", { class: "grid" }, head, missing, what), el("h2", { class: "section", text: "Security baseline" }), list, notes ? el("div", { style: null }, el("br"), notes) : null);
}

// --------------------------------------------------------------- sessions

function sessionsTab(s) {
  const se = s.sessions;
  const rows = se.current.map((x) => el("tr", {},
    el("td", {}, el("code", { text: x.id })), el("td", { text: x.template }), el("td", { text: x.state }),
    el("td", { text: x.cpus ? x.cpus + " threads" : "" }), el("td", { text: x.since ? ago(x.since) : "" })));
  return el("div", { class: "grid" },
    card("Running now",
      se.current.length
        ? el("table", { class: "sessions" }, el("thead", {}, el("tr", {}, ["Session", "App", "State", "CPU", "Started"].map((h) => el("th", { text: h })))), el("tbody", {}, rows))
        : el("p", { class: "muted", text: "No sessions are running on this machine." }),
      el("p", { class: "muted small", text: "Only what runs where is shown: never what a consumer sends or receives." })),
    card("Since this start",
      kv([
        ["Sessions hosted", String(se.hosted)],
        ["Machine time", dur(se.session_seconds)],
        ["Chat jobs", String(s.jobs.done) + (s.jobs.failed ? " (" + s.jobs.failed + " failed)" : "")],
        ["Running jobs", String(s.jobs.active)],
        ["Functions", String(s.delivered.functions)],
        ["Vector operations", String(s.delivered.vector_ops)],
      ])));
}

// --------------------------------------------------------------- earnings

function earningsTab(s) {
  const e = s.earnings;
  if (!e || !e.enabled) {
    return el("div", { class: "grid" }, card("Earnings",
      el("p", { text: "This fabric does not report KMPLIFY Credits for this node (the credit system is off here, or the node has not been answered yet)." }),
      el("p", {}, "How earning works: ", el("a", { href: "https://kmplify.io/docs", target: "_blank", rel: "noopener noreferrer", text: "kmplify.io/docs" })),
      kv([["Delivered since start", s.delivered.jobs + s.delivered.functions + s.delivered.vector_ops + " calls"]])));
  }
  return el("div", { class: "grid" },
    card("This month", el("div", { class: "big", text: String(e.earned_this_month) }), el("div", { class: "muted", text: "credits, after the platform fee" })),
    card("In total", el("div", { class: "big", text: String(e.earned_total) }), el("div", { class: "muted", text: "credits" })),
    card("Account and rate",
      kv([
        ["Account", e.linked ? "linked" : "NOT LINKED: earnings are held until this node is linked"],
        ["Held", e.held ? String(e.held) + " credits" : ""],
        ["GPU class", e.gpu_class ? e.gpu_class.toUpperCase() + (e.vram_gb ? " (" + e.vram_gb + " GB)" : "") : "none"],
        ["Rate", e.credits_per_minute !== null && e.credits_per_minute !== undefined ? e.credits_per_minute + " credits per minute" : ""],
        ["Platform fee", e.platform_fee_percent !== null && e.platform_fee_percent !== undefined ? e.platform_fee_percent + " %" : ""],
      ])));
}

// --------------------------------------------------------------- settings

function control(x) {
  if (!x.editable) return el("div", { class: "ctl" });
  const ctl = el("div", { class: "ctl" });
  const save = (value) => act("/api/settings", { key: x.key, value });
  if (x.kind === "bool") {
    ctl.append(el("button", { class: "btn" + (x.value ? " ghost" : ""), onclick: () => save(!x.value) }, x.value ? "Turn off" : "Turn on"));
  } else if (x.kind === "choice") {
    const sel = el("select", { "aria-label": x.label, onfocus: () => { editing = true; }, onblur: () => { editing = false; } },
      ["auto", "manual"].map((o) => el("option", { value: o, selected: o === x.value, text: o })));
    sel.addEventListener("change", () => save(sel.value));
    ctl.append(sel);
  } else {
    const inp = el("input", { type: "text", value: x.value === null || x.value === undefined ? "" : String(x.value), "aria-label": x.label,
      onfocus: () => { editing = true; }, onblur: () => { editing = false; } });
    ctl.append(inp, el("button", { class: "btn", onclick: () => { editing = false; save(inp.value); } }, "Save"));
  }
  if (x.overridden) {
    ctl.append(el("button", { class: "btn ghost", onclick: () => act("/api/settings/clear", { key: x.key }) }, "Use environment"));
  }
  return ctl;
}

function settingsTab(s) {
  const box = card("Settings",
    el("p", { class: "muted small", text: "Changes here are the same as kmplify-node set: saved in the node directory, applied without a restart (the node reconnects to re-advertise), and they win over the environment until you reset them." }));
  for (const x of s.settings) {
    const shown = x.value === null || x.value === undefined || x.value === "" ? "not set" : String(x.value);
    box.append(el("div", { class: "setting" },
      el("div", {},
        el("strong", { text: x.label }),
        el("div", { class: "small" }, "Now: ", el("code", { text: shown }),
          x.overridden ? el("span", { class: "chip warn", text: "set here, overrides " + x.var }) : el("span", { class: "muted", text: " (" + x.var + ")" })),
        el("div", { class: "small muted" }, "Change: ", el("code", { text: x.how }))),
      control(x)));
  }
  return box;
}

// ------------------------------------------------------------------- logs

function logsTab(s) {
  const pre = el("pre", { class: "log", "aria-label": "Log" });
  const lines = s.logs.filter((l) => !logFilter || l.toLowerCase().includes(logFilter.toLowerCase()));
  pre.textContent = lines.join("\n") || "No log lines yet.";
  const filter = el("input", { class: "filter", type: "search", placeholder: "Filter", value: logFilter, "aria-label": "Filter log",
    onfocus: () => { editing = true; }, onblur: () => { editing = false; } });
  filter.addEventListener("input", () => { logFilter = filter.value; const p = document.querySelector("pre.log"); if (p) p.textContent = s.logs.filter((l) => !logFilter || l.toLowerCase().includes(logFilter.toLowerCase())).join("\n"); });
  setTimeout(() => { pre.scrollTop = pre.scrollHeight; }, 0);
  return card("Last log lines", el("p", { class: "muted small", text: "The newest 200 lines of this run. Anything that looks like a key or token is replaced by [redacted]." }), filter, pre);
}

// ----------------------------------------------------------------- frame

const TABS = { overview, trust: trustTab, sessions: sessionsTab, earnings: earningsTab, settings: settingsTab, logs: logsTab };

function render() {
  if (!state) return;
  header(state);
  const sec = document.getElementById("tab-" + current);
  sec.replaceChildren(TABS[current](state));
}

function show(name) {
  current = name;
  for (const b of document.querySelectorAll(".tabs button")) {
    b.setAttribute("aria-selected", b.dataset.tab === name ? "true" : "false");
  }
  for (const s of document.querySelectorAll(".tab")) s.hidden = s.id !== "tab-" + name;
  editing = false;
  render();
  window.scrollTo(0, 0);
}

async function refresh() {
  const banner = document.getElementById("banner");
  try {
    state = await api("/api/state");
    banner.hidden = true;
    if (!editing) render();
  } catch (e) {
    banner.hidden = false;
    banner.textContent = "The node is not answering (" + e.message + "). It may have stopped; start it again with kmplify-node.";
  }
}

for (const b of document.querySelectorAll(".tabs button")) {
  b.addEventListener("click", () => show(b.dataset.tab));
}
if (location.hash && TABS[location.hash.slice(1)]) show(location.hash.slice(1));
refresh();
setInterval(refresh, 3000);
