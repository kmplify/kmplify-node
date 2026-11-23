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

// ------------------------------------------------------------------ theme
// Follows the system (prefers-color-scheme) until the toggle is used; a
// choice that equals the system's is forgotten, so the page follows it again.
const THEME_KEY = "kn-theme";
const darkQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: dark)") : null;
let themeChoice = null;
try {
  const v = localStorage.getItem(THEME_KEY);
  if (v === "light" || v === "dark") themeChoice = v;
} catch (_) { themeChoice = null; }

function applyTheme() {
  const root = document.documentElement;
  const dark = themeChoice ? themeChoice === "dark" : !!(darkQuery && darkQuery.matches);
  if (themeChoice) root.dataset.theme = themeChoice; else delete root.dataset.theme;
  root.toggleAttribute("data-dark", dark);
  const b = document.getElementById("theme-toggle");
  if (b) {
    const label = dark ? "Switch to light theme" : "Switch to dark theme";
    b.setAttribute("aria-label", label);
    b.title = label;
  }
}

function toggleTheme() {
  const next = document.documentElement.hasAttribute("data-dark") ? "light" : "dark";
  const system = darkQuery && darkQuery.matches ? "dark" : "light";
  themeChoice = next === system ? null : next;
  try {
    if (themeChoice) localStorage.setItem(THEME_KEY, themeChoice); else localStorage.removeItem(THEME_KEY);
  } catch (_) { /* private window: the choice holds for this page only */ }
  applyTheme();
}

applyTheme();
if (darkQuery && darkQuery.addEventListener) darkQuery.addEventListener("change", applyTheme);
document.getElementById("theme-toggle").addEventListener("click", toggleTheme);

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

// An embedded icon (index.html, <template id="kn-icons">), cloned; no markup
// is ever built from strings.
function icon(name) {
  const t = document.getElementById("kn-icons");
  const s = t && t.content.querySelector('[data-i="' + name + '"]');
  return s ? s.cloneNode(true) : null;
}

// A card with the Tailux icon tile next to its title.
function cardI(name, title, ...kids) {
  return el("div", { class: "card" },
    el("div", { class: "card-head" }, el("span", { class: "card-ico", "aria-hidden": "true" }, icon(name)), el("h2", { text: title })),
    ...kids);
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

// -------------------------------------------------------------- readiness

const ITEM_MARK = { pass: "✓", not_applicable: "✓", fail: "✕", unknown: "?", checked: "✓", checking: "…" };
const ITEM_WORD = { pass: "passes", not_applicable: "not needed here", fail: "fails", unknown: "unknown, counts as failing", checked: "read", checking: "checking" };
const STEP_WORD = { done: "done", missing: "to do", open: "not confirmed", checking: "checking" };

// The ring: the share of the 6 items that pass (or, while the first check
// runs, that were read), coloured by band, with the count in the middle.
// The verdict next to it says the same in words.
function ring(rd) {
  const n = rd.band === "none" ? rd.checked : rd.passed;
  const r = el("div", { class: "ring band-" + rd.band + (rd.checking ? " busy" : ""), "aria-hidden": "true" },
    el("span", { class: "ring-num", text: n + "/" + rd.total }),
    el("span", { class: "ring-sub", text: rd.band === "none" ? "read" : "checks" }));
  r.style.setProperty("--p", String(Math.round(100 * n / rd.total)));
  return r;
}

function readiness(rd, compact) {
  const head = el("div", { class: "ready-head" }, ring(rd),
    el("div", { class: "grow" },
      el("div", { class: "ready-title" }, "Trust readiness ",
        el("span", { class: "chip " + ({ green: "ok", amber: "warn", red: "bad" }[rd.band] || ""),
          text: { green: "ready", amber: "almost", red: "not ready", none: "checking" }[rd.band] })),
      el("p", { class: "verdict", role: "status", "aria-live": "polite", text: rd.verdict }),
      rd.checking ? el("div", { class: "meter busy", role: "progressbar", "aria-label": "Security check progress",
        "aria-valuemin": "0", "aria-valuemax": String(rd.total), "aria-valuenow": String(rd.checked),
        "aria-valuetext": rd.checked + " of " + rd.total + " checks read" }, (() => { const b = el("span"); b.style.width = (100 * rd.checked / rd.total) + "%"; return b; })()) : null));
  const items = el("ul", { class: "ready-items", "aria-label": "Baseline checks" },
    rd.items.map((i) => el("li", { class: "ri " + i.state },
      el("span", { class: "ri-mark", "aria-hidden": "true", text: ITEM_MARK[i.state] || "?" }),
      el("span", { text: i.label }),
      el("span", { class: "sr", text: ": " + (ITEM_WORD[i.state] || i.state) }))));
  const steps = el("ol", { class: "steps", "aria-label": "Conditions for Trusted" },
    rd.steps.map((st, n) => el("li", { class: "step " + st.state },
      el("span", { class: "step-n", "aria-hidden": "true", text: st.state === "done" ? "✓" : String(n + 1) }),
      el("div", { class: "grow" },
        el("div", {}, el("strong", { text: st.label }), " ", el("span", { class: "chip " + ({ done: "ok", missing: "bad", open: "warn" }[st.state] || ""), text: STEP_WORD[st.state] || st.state })),
        compact ? null : el("div", { class: "small muted", text: st.text })))));
  return el("div", { class: "ready" }, head, items, steps);
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
  const rd = s.trust.readiness, fab = s.trust.fabric;
  trust.textContent = (st.level === "unknown" && fab && fab.state === "unreachable" ? "Fabric not reachable" : LEVEL_TEXT[st.level] || st.level)
    + (rd && rd.band !== "none" && st.level !== "cloud" ? " | " + rd.passed + "/" + rd.total + " checks" : rd && rd.checking ? " | checking" : "");
  viewSwitch(s.view);
  const badge = document.getElementById("badge-trust");
  const bad = s.trust.checklist.filter((i) => i.state === "fail" || i.state === "unknown").length;
  badge.hidden = bad === 0;
  badge.textContent = String(bad);
}

// ------------------------------------------------------------------- view

// Web / App window / Terminal: the same views the console keys open. Shown
// only when a person started this node (a service has no views to switch).
function viewSwitch(v) {
  const box = document.getElementById("view-switch");
  box.hidden = !(v && v.switchable);
  if (box.hidden) return;
  for (const b of box.querySelectorAll("button")) {
    const which = b.dataset.view;
    b.setAttribute("aria-pressed", which === "terminal" ? String(!!v.terminal_up) : "false");
    b.disabled = which === "terminal" && !v.console;
  }
}

// --------------------------------------------------------------- overview

function overview(s) {
  const n = s.node, m = s.machine, sh = s.sharing, st = s.trust.standing;
  const conn = cardI("signal", "Connection",
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
      el("button", { class: "btn ghost", onclick: () => act("/api/reconnect") }, "Reconnect now"),
      el("button", { class: "btn ghost", onclick: () => act("/api/report") }, "Save a report"),
      el("button", { class: "btn ghost", onclick: () => {
        if (window.confirm("Stop this node? Hosted sessions are torn down first.")) act("/api/shutdown");
      } }, "Stop the node")),
    n.paused ? el("p", { class: "muted small", text: "Paused: the connection and running sessions stay up; nothing new is offered until you resume." }) : null);

  const models = sh.inference && sh.models.length === 0;
  const shares = cardI("share", "What this node shares",
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

  const machine = cardI("cpu", "This machine",
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
  const trust = cardI("trust", "Trusted node",
    el("p", { class: "lead" }, el("span", { class: "pill " + (st.level === "trusted" || st.level === "cloud" ? "ok" : st.level === "community" ? "warn" : ""), text: LEVEL_TEXT[st.level] || st.level }), " ", st.headline),
    s.trust.readiness ? readiness(s.trust.readiness, true) : null,
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
    t.readiness ? readiness(t.readiness, false) : null,
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
    el("td", { text: x.cpus ? x.cpus + " threads" : "" }), el("td", { text: x.since ? ago(x.since) : "" }),
    el("td", {}, el("button", { class: "btn ghost", "aria-label": "Stop session " + x.id, onclick: () => {
      if (window.confirm("Stop session " + x.id + " and remove its container?")) act("/api/session/stop", { id: x.session_id });
    } }, "Stop session"))));
  return el("div", { class: "grid" },
    card("Running now",
      se.current.length
        ? el("table", { class: "sessions" }, el("thead", {}, el("tr", {}, ["Session", "App", "State", "CPU", "Started", ""].map((h) => el("th", { text: h })))), el("tbody", {}, rows))
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

// -------------------------------------------------------------- link code
// Made on request only and kept on this page, never in /api/state: a code is
// a one-time proof for linking this machine to an account, not a fact about
// the node. The same code as `kmplify-node link-code` and the terminal view.
let linkCode = null;

async function makeLinkCode() {
  const box = document.getElementById("link-nonce");
  const nonce = box ? box.value.trim() : "";
  try {
    const r = await api("/api/link-code", nonce ? { nonce } : {});
    if (r.ok) linkCode = r; else toast(r.message || "No link code was made.");
  } catch (e) {
    toast("The node did not answer: " + e.message);
  }
  render();
}

async function copyLinkCode() {
  try {
    await navigator.clipboard.writeText(linkCode.code);
    toast("Link code copied.");
  } catch (_) {
    toast("Copying was refused by the browser; select the code and copy it.");
  }
}

function linkCard() {
  const live = linkCode && linkCode.expires_at > Date.now() / 1000;
  return card("Link code",
    el("p", { class: "small", text: "Links this machine to your KMPLIFY account, so the marketplace shows your @alias on its card. Enter the code in the KMPLIFY marketplace: Settings > Peer Compute > Link code." }),
    live ? el("p", {}, el("code", { text: linkCode.code })) : null,
    live
      ? el("p", { class: "muted small", text: "Valid until " + new Date(linkCode.expires_at * 1000).toLocaleTimeString() + ", once. Signed by this node's identity key; it carries no personal data." })
      : el("p", { class: "muted small", text: linkCode ? "That code has expired. Make a new one." : "A code is valid for 10 minutes and can be used once." }),
    live ? (linkCode.lines || []).slice(3).map((l) => el("p", { class: "small warn-text", text: l })) : null,
    !live ? el("p", { class: "muted small", text: "Without a challenge the code is not bound to an account: whoever has it within 10 minutes can redeem it. Binding is recommended: make the code in KMPLIFY Desktop (signed in), or paste the challenge your account shows (marketplace: Settings > Peer Compute)." }) : null,
    el("label", { class: "small", for: "link-nonce", text: "Challenge from your account (optional)" }),
    el("input", { id: "link-nonce", type: "text", autocomplete: "off", spellcheck: "false", maxlength: "64", onfocus: () => { editing = true; }, onblur: () => { editing = false; } }),
    el("div", { class: "actions" },
      el("button", { class: "btn", onclick: makeLinkCode }, "Make a link code"),
      live ? el("button", { class: "btn ghost", onclick: copyLinkCode }, "Copy") : null));
}

// --------------------------------------------------------------- earnings

function earningsTab(s) {
  const e = s.earnings;
  if (!e || !e.enabled) {
    return el("div", { class: "grid" }, linkCard(), card("Earnings",
      el("p", { text: "This fabric does not report KMPLIFY Credits for this node (the credit system is off here, or the node has not been answered yet)." }),
      el("p", {}, "How earning works: ", el("a", { href: "https://kmplify.io/docs", target: "_blank", rel: "noopener noreferrer", text: "kmplify.io/docs" })),
      kv([["Delivered since start", s.delivered.jobs + s.delivered.functions + s.delivered.vector_ops + " calls"]]),
      el("p", { class: "muted small", text: "This month, In total and Account and rate appear once the fabric reports credits." })));
  }
  return el("div", { class: "grid" },
    linkCard(),
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
  const tab = document.querySelector('.tabs button[data-tab="' + name + '"] .lbl');
  if (tab) document.getElementById("page-title").textContent = tab.textContent;
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
for (const b of document.querySelectorAll("#view-switch button")) {
  b.addEventListener("click", () => act("/api/view", { view: b.dataset.view }));
}
if (location.hash && TABS[location.hash.slice(1)]) show(location.hash.slice(1));
// Every 3 s; every second while a security check runs, so the meter moves.
async function loop() {
  await refresh();
  const busy = state && state.trust && state.trust.readiness && state.trust.readiness.checking;
  setTimeout(loop, busy ? 1000 : 3000);
}
loop();
