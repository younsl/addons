"use strict";

const POLL_MS = 5000;

const ACTIONS = {
  in_sync: { label: "In sync", kind: "ok" },
  create: { label: "Created", kind: "change", planned: "Create" },
  relink: { label: "Relinked", kind: "change", planned: "Relink" },
  make_dir: { label: "Directory made", kind: "change", planned: "Make directory" },
  backup: { label: "Backed up", kind: "backup", planned: "Back up" },
  missing_source: { label: "Missing source", kind: "missing" },
};

const FILTERS = [
  { id: "all", label: "All", match: () => true },
  { id: "changed", label: "Changed", match: (e) => e.applied || (isPlanned(e) && !e.error) },
  { id: "failed", label: "Failed", match: (e) => Boolean(e.error) },
  { id: "missing", label: "Missing source", match: (e) => e.action === "missing_source" },
  { id: "in_sync", label: "In sync", match: (e) => e.action === "in_sync" && !e.error },
];

const state = {
  info: null,
  report: null,
  filter: "all",
  query: "",
  lastOk: 0,
  busy: false,
};

const $ = (id) => document.getElementById(id);

function isPlanned(entry) {
  return state.report?.dry_run && ACTIONS[entry.action]?.planned !== undefined;
}

function tilde(path) {
  const home = state.info?.home;
  if (!home || !path) return path ?? "";
  if (path === home) return "~";
  return path.startsWith(home + "/") ? "~" + path.slice(home.length) : path;
}

function sourceRoot(entries) {
  const dirs = entries.map((e) => e.source.split("/").slice(0, -1));
  if (dirs.length === 0) return "";
  let common = dirs[0];
  for (const d of dirs.slice(1)) {
    let i = 0;
    while (i < common.length && i < d.length && common[i] === d[i]) i++;
    common = common.slice(0, i);
  }
  return common.join("/");
}

function relative(path, root) {
  return root && path.startsWith(root + "/") ? path.slice(root.length + 1) : tilde(path);
}

function ago(date) {
  const s = Math.max(0, Math.round((Date.now() - date.getTime()) / 1000));
  if (s < 5) return "just now";
  if (s < 60) return `${s} seconds ago`;
  const m = Math.round(s / 60);
  if (m < 60) return m === 1 ? "a minute ago" : `${m} minutes ago`;
  const h = Math.round(m / 60);
  return h === 1 ? "an hour ago" : `${h} hours ago`;
}

function until(seconds) {
  if (seconds <= 5) return "any moment";
  if (seconds < 60) return `in ${seconds} seconds`;
  const m = Math.round(seconds / 60);
  return m === 1 ? "in a minute" : `in ${m} minutes`;
}

function plural(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

let toastTimer;
function toast(message, kind = "info") {
  const el = $("toast");
  el.textContent = message;
  el.dataset.kind = kind;
  el.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, 4000);
}

async function getJson(path) {
  const res = await fetch(path, { cache: "no-store" });
  if (!res.ok) throw new Error(`${path} returned ${res.status}`);
  return res.json();
}

function renderBuild() {
  const i = state.info;
  if (!i) return;
  $("build").textContent = `v${i.version} ${i.commit}`;
  const facts = [
    ["Config file", i.config_file],
    ["Home", i.home],
    ["Reconcile interval", `${Math.round(i.reconcile_interval_secs / 60)} minutes`],
    ["Mode", i.dry_run ? "Dry run, nothing is changed" : "Apply changes"],
    ["Build", `${i.version}, commit ${i.commit}, rustc ${i.rustc}`],
  ];
  const dl = $("facts");
  dl.replaceChildren();
  for (const [term, value] of facts) {
    const wrap = document.createElement("div");
    const dt = document.createElement("dt");
    const dd = document.createElement("dd");
    dt.textContent = term;
    dd.textContent = value;
    wrap.append(dt, dd);
    dl.append(wrap);
  }
}

function renderHealth(ok) {
  const el = $("health");
  const text = $("health-text");
  if (!ok) {
    el.dataset.state = "down";
    text.textContent = "Unreachable";
  } else if (state.report?.error) {
    el.dataset.state = "stale";
    text.textContent = "Config error";
  } else {
    el.dataset.state = "ready";
    text.textContent = "Ready";
  }
}

function renderStatus() {
  const r = state.report;
  const band = $("status");
  const headline = $("headline");
  const lede = $("lede");
  if (state.busy) band.dataset.tone = "busy";
  if (!r) return;

  const started = new Date(r.started_at);
  const interval = state.info?.reconcile_interval_secs ?? 300;
  const next = Math.max(0, Math.round(interval - (Date.now() - started.getTime()) / 1000));
  const when = `Checked ${ago(started)}. Next check ${until(next)}.`;

  if (r.error) {
    if (!state.busy) band.dataset.tone = "bad";
    headline.textContent = "The config file could not be loaded";
    lede.textContent = `${r.error} Fix the file and tether picks it up on the next check.`;
    return;
  }

  const total = r.entries.length;
  const failed = r.entries.filter((e) => e.error).length;
  const missing = r.entries.filter((e) => e.action === "missing_source").length;
  const changed = r.entries.filter((e) => e.applied).length;
  const pending = r.entries.filter((e) => isPlanned(e)).length;

  if (!state.busy) band.dataset.tone = failed > 0 ? "bad" : "ok";
  if (failed > 0) {
    headline.textContent = `${plural(failed, "link", "links")} failed to apply`;
  } else if (r.dry_run && pending > 0) {
    headline.textContent = `${plural(pending, "link needs", "links need")} a change`;
  } else if (total === 0) {
    headline.textContent = "No links are configured";
  } else {
    headline.textContent = `All ${plural(total, "link is", "links are")} in place`;
  }

  const notes = [];
  if (changed > 0) notes.push(`${plural(changed, "link was", "links were")} updated.`);
  if (r.dry_run && pending > 0) notes.push("Dry run is on, so nothing was changed.");
  if (missing > 0) notes.push(`${plural(missing, "source is", "sources are")} missing and left alone.`);
  if (r.backup_dir) notes.push(`Replaced files were moved to ${tilde(r.backup_dir)}.`);
  if ((r.packages ?? []).some((f) => f.missing_mount)) notes.push("The Brewfile is not being refreshed. Package files below shows how to fix it.");
  lede.textContent = [...notes, when].join(" ");
}

function renderFilters() {
  const entries = state.report?.entries ?? [];
  const bar = $("filters");
  bar.replaceChildren();
  for (const f of FILTERS) {
    const count = entries.filter(f.match).length;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "pill";
    btn.setAttribute("role", "tab");
    btn.setAttribute("aria-selected", String(state.filter === f.id));
    btn.disabled = f.id !== "all" && count === 0 && state.filter !== f.id;
    const label = document.createElement("span");
    label.textContent = f.label;
    const c = document.createElement("span");
    c.className = "count";
    c.textContent = String(count);
    btn.append(label, c);
    btn.addEventListener("click", () => {
      state.filter = f.id;
      renderFilters();
      renderRows();
    });
    bar.append(btn);
  }
}

function badgeFor(entry) {
  const meta = ACTIONS[entry.action] ?? { label: entry.action, kind: "ok" };
  const span = document.createElement("span");
  span.className = "badge";
  if (entry.error) {
    span.dataset.kind = "failed";
    span.textContent = "Failed";
  } else if (isPlanned(entry)) {
    span.dataset.kind = meta.kind;
    span.textContent = `${meta.planned} planned`;
  } else {
    if (meta.kind !== "ok") span.dataset.kind = meta.kind;
    span.textContent = meta.label;
  }
  return span;
}

const LANGUAGES = [
  [/(^|\/)Brewfile$|\.rb$/, "ruby"],
  [/\.rs$/, "rust"],
  [/\.lua$/, "lua"],
  [/\.ya?ml$/, "yaml"],
  [/\.json$/, "json"],
  [/\.md$/, "markdown"],
  [/(^|\/)Makefile$/, "makefile"],
  [/\.(sh|zsh|bash)$|(^|\/)\.(zshrc|bashrc|profile)$|\/zsh\/functions\//, "bash"],
  [/\.(toml|ini|conf|cfg|lock)$|(^|\/)\.(npmrc|yarnrc)$|\/git\/(config|config-younsl|ignore|gitmessage)$|\/ghostty\/config$/, "ini"],
];

function languageFor(path, content) {
  for (const [pattern, lang] of LANGUAGES) if (pattern.test(path)) return lang;
  if (/^#!.*\b(ba|z)?sh\b/.test(content)) return "bash";
  return "plaintext";
}

const viewer = { opener: null, root: null };

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function renderCode(path, content) {
  const box = $("viewer-code");
  box.replaceChildren();
  const lines = content.endsWith("\n") ? content.slice(0, -1).split("\n") : content.split("\n");
  const gutter = document.createElement("pre");
  gutter.className = "gutter";
  gutter.setAttribute("aria-hidden", "true");
  gutter.textContent = lines.map((_, i) => i + 1).join("\n");
  const pre = document.createElement("pre");
  pre.className = "source";
  const code = document.createElement("code");
  const lang = languageFor(path, content);
  code.className = `hljs language-${lang}`;
  if (window.hljs && lang !== "plaintext") {
    code.innerHTML = window.hljs.highlight(lines.join("\n"), { language: lang, ignoreIllegals: true }).value;
  } else {
    code.textContent = lines.join("\n");
  }
  pre.append(code);
  box.append(gutter, pre);
  box.scrollTop = 0;
}

function notice(message) {
  const p = document.createElement("p");
  p.className = "viewer-notice";
  p.textContent = message;
  $("viewer-code").replaceChildren(p);
}

async function showFile(path) {
  for (const b of $("viewer-tree").querySelectorAll("button")) {
    b.setAttribute("aria-current", String(b.dataset.path === path));
  }
  const relative = viewer.root && path.startsWith(viewer.root + "/") ? path.slice(viewer.root.length + 1) : tilde(path);
  $("viewer-path").textContent = relative;
  notice("Loading");
  try {
    const file = await getJson(`/api/file?path=${encodeURIComponent(path)}`);
    if (file.content === undefined) {
      notice(`This file is not shown because it is binary, not UTF-8, or larger than 1 MB (${formatSize(file.size)}).`);
    } else if (file.content === "") {
      notice("This file is empty.");
    } else {
      renderCode(path, file.content);
    }
  } catch {
    notice("This file is no longer in the last reconcile. Close the viewer and try again.");
  }
}

async function openViewer(root, title, opener) {
  viewer.opener = opener;
  viewer.root = root;
  $("viewer-title").textContent = title;
  $("viewer-sub").textContent = tilde(root);
  $("viewer-tree").replaceChildren();
  $("viewer-path").textContent = "";
  notice("Loading");
  $("viewer").hidden = false;
  document.body.classList.add("locked");
  $("viewer-close").focus();

  let tree;
  try {
    tree = await getJson(`/api/tree?path=${encodeURIComponent(root)}`);
  } catch {
    notice("This source is no longer in the last reconcile. Close the viewer and try again.");
    return;
  }
  const single = tree.files.length === 1 && tree.files[0].path === root;
  $("viewer").dataset.single = String(single);
  if (tree.files.length === 0) {
    notice("Nothing to show. Only files tracked in git appear here, so local-only files such as work settings and keys stay hidden.");
    return;
  }
  const nav = $("viewer-tree");
  for (const f of tree.files) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.dataset.path = f.path;
    const name = document.createElement("span");
    name.textContent = f.path.startsWith(root + "/") ? f.path.slice(root.length + 1) : f.path.split("/").pop();
    const size = document.createElement("span");
    size.className = "size";
    size.textContent = formatSize(f.size);
    btn.append(name, size);
    btn.addEventListener("click", () => showFile(f.path));
    nav.append(btn);
  }
  showFile(tree.files[0].path);
}

function closeViewer() {
  if ($("viewer").hidden) return;
  $("viewer").hidden = true;
  document.body.classList.remove("locked");
  viewer.opener?.focus();
}

const ROUTES = { "/": "page-overview", "/logs": "page-logs" };

function onLogsPage() {
  return location.pathname === "/logs";
}

function route() {
  const page = ROUTES[location.pathname] ?? "page-overview";
  for (const id of Object.values(ROUTES)) $(id).hidden = id !== page;
  for (const link of document.querySelectorAll(".nav-link")) {
    if (link.dataset.route === location.pathname) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
  document.title = onLogsPage() ? "tether logs" : "tether";
  $("facts").parentElement.hidden = onLogsPage();
  if (onLogsPage()) {
    renderLogFilters();
    renderLogLines(true);
  }
}

function navigate(path) {
  if (path !== location.pathname) history.pushState(null, "", path);
  route();
  window.scrollTo(0, 0);
}

const LOG_LEVELS = [
  { id: "all", label: "All", match: () => true },
  { id: "error", label: "Error", match: (r) => r.level === "error" },
  { id: "warn", label: "Warn", match: (r) => r.level === "warn" },
  { id: "info", label: "Info", match: (r) => r.level === "info" },
  { id: "debug", label: "Debug", match: (r) => r.level === "debug" || r.level === "trace" },
];
const logs = { records: [], last: 0, level: "all" };

function clockTime(iso) {
  const d = new Date(iso);
  return [d.getHours(), d.getMinutes(), d.getSeconds()].map((n) => String(n).padStart(2, "0")).join(":");
}

const COPY_ICON = '<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5" fill="none" stroke="currentColor" stroke-width="1.3"/><path d="M10.5 3.5V3a1.5 1.5 0 0 0-1.5-1.5H3.5A1.5 1.5 0 0 0 2 3v5.5A1.5 1.5 0 0 0 3.5 10H4" fill="none" stroke="currentColor" stroke-width="1.3"/></svg>';

function logText(r) {
  const fields = r.fields.map(([k, v]) => `${k}=${v}`).join(" ");
  return `${r.time} ${r.level.toUpperCase()} ${r.message}${fields ? " " + fields : ""}`;
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast("Copied to the clipboard.");
  } catch {
    toast("Copy failed. Select the text and copy it by hand.", "error");
  }
}

function logLine(r) {
  const li = document.createElement("li");
  li.className = "log-line";
  li.dataset.level = r.level;
  const time = document.createElement("time");
  time.dateTime = r.time;
  time.textContent = clockTime(r.time);
  const level = document.createElement("span");
  level.className = "log-level";
  level.textContent = r.level;
  const text = document.createElement("span");
  text.className = "log-text";
  const msg = document.createElement("span");
  msg.className = "log-message";
  msg.textContent = r.message;
  text.append(msg);
  for (const [key, value] of r.fields) {
    const field = document.createElement("span");
    field.className = "log-field";
    const k = document.createElement("span");
    k.className = "log-key";
    k.textContent = `${key}=`;
    field.append(k, document.createTextNode(tilde(value)));
    text.append(" ", field);
  }
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "log-copy";
  copy.setAttribute("aria-label", "Copy this log line");
  copy.innerHTML = COPY_ICON;
  copy.addEventListener("click", () => copyText(logText(r)));
  li.append(time, level, text, copy);
  return li;
}

function renderLogFilters() {
  const bar = $("logs-filters");
  bar.replaceChildren();
  for (const f of LOG_LEVELS) {
    const count = logs.records.filter(f.match).length;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "pill";
    btn.setAttribute("role", "tab");
    btn.setAttribute("aria-selected", String(logs.level === f.id));
    const label = document.createElement("span");
    label.textContent = f.label;
    const c = document.createElement("span");
    c.className = "count";
    c.textContent = String(count);
    btn.append(label, c);
    btn.addEventListener("click", () => {
      logs.level = f.id;
      renderLogFilters();
      renderLogLines(true);
    });
    bar.append(btn);
  }
}

function renderLogLines(reset, added = []) {
  const list = $("log-lines");
  const filter = LOG_LEVELS.find((f) => f.id === logs.level) ?? LOG_LEVELS[0];
  if (reset) list.replaceChildren(...logs.records.filter(filter.match).map(logLine));
  else list.append(...added.filter(filter.match).map(logLine));
  while (list.children.length > 1000) list.firstChild.remove();
  const empty = $("logs-empty");
  empty.hidden = list.children.length > 0;
  empty.textContent = logs.records.length === 0 ? "No events yet." : "No events at this level.";
  if ($("logs-follow").checked) $("logs-body").scrollTop = $("logs-body").scrollHeight;
}

function renderLogAlert() {
  const alerts = logs.records.filter((r) => r.level === "warn" || r.level === "error").length;
  const badge = $("logs-alert");
  badge.hidden = alerts === 0;
  badge.textContent = String(alerts);
  badge.dataset.kind = logs.records.some((r) => r.level === "error") ? "error" : "warn";
}

async function pollLogs() {
  let added;
  try {
    added = await getJson(`/api/logs?after=${logs.last}`);
  } catch {
    return;
  }
  if (added.length === 0) return;
  logs.last = added[added.length - 1].seq;
  logs.records.push(...added);
  if (logs.records.length > 1000) logs.records.splice(0, logs.records.length - 1000);
  renderLogAlert();
  if (onLogsPage()) {
    renderLogFilters();
    renderLogLines(false, added);
  }
}


function renderRows() {
  const entries = state.report?.entries ?? [];
  const root = sourceRoot(entries);
  const filter = FILTERS.find((f) => f.id === state.filter) ?? FILTERS[0];
  const q = state.query.trim().toLowerCase();
  const rows = entries.filter(filter.match).filter((e) =>
    !q || e.target.toLowerCase().includes(q) || e.source.toLowerCase().includes(q));

  const tbody = $("rows");
  tbody.replaceChildren();
  for (const e of rows) {
    const tr = document.createElement("tr");
    const target = document.createElement("td");
    const open = document.createElement("button");
    open.type = "button";
    open.className = "row-link";
    open.textContent = tilde(e.target);
    open.addEventListener("click", () => openViewer(e.source, tilde(e.target), open));
    target.append(open);
    const source = document.createElement("td");
    source.className = "source";
    source.textContent = relative(e.source, root);
    const action = document.createElement("td");
    action.append(badgeFor(e));
    const detail = document.createElement("td");
    detail.className = "detail";
    if (e.error) {
      detail.classList.add("error");
      detail.textContent = e.error;
    } else if (e.backup) {
      detail.textContent = `Moved to ${tilde(e.backup)}`;
    }
    tr.append(target, source, action, detail);
    tbody.append(tr);
  }

  const empty = $("empty");
  if (!state.report) {
    empty.textContent = "Links appear here after the first reconcile.";
  } else if (entries.length === 0) {
    empty.textContent = "The config file declares no links.";
  } else if (rows.length === 0) {
    empty.textContent = q ? `No link matches "${state.query.trim()}".` : "No link in this group.";
  }
  empty.hidden = rows.length > 0;
}

function renderPackages() {
  const files = state.report?.packages ?? [];
  const panel = $("packages-panel");
  panel.hidden = files.length === 0;
  const list = $("packages");
  list.replaceChildren();
  for (const f of files) {
    const li = document.createElement("li");
    const main = document.createElement("div");
    const path = document.createElement("button");
    path.type = "button";
    path.className = "path row-link";
    path.textContent = tilde(f.path);
    path.addEventListener("click", () => openViewer(f.path, f.path.split("/").pop(), path));
    main.append(path);
    const entries = document.createElement("span");
    entries.className = "entries";
    entries.textContent = plural(f.entries, "entry", "entries");
    const badge = document.createElement("span");
    badge.className = "badge";
    if (f.error) {
      badge.dataset.kind = "failed";
      badge.textContent = "Failed";
      const err = document.createElement("p");
      err.className = "path-error";
      err.textContent = f.error;
      main.append(err);
    } else if (f.updated) {
      badge.dataset.kind = "change";
      badge.textContent = state.report.dry_run ? "Update planned" : "Updated";
    } else {
      badge.textContent = "Unchanged";
    }
    li.append(main, entries, badge);
    list.append(li);
  }
}

function codeBlock(text) {
  const wrap = document.createElement("div");
  wrap.className = "code";
  const pre = document.createElement("pre");
  pre.textContent = text;
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "btn btn-ghost";
  copy.textContent = "Copy";
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(text);
      toast("Copied to the clipboard.");
    } catch {
      toast("Copy failed. Select the text and copy it by hand.", "error");
    }
  });
  wrap.append(pre, copy);
  return wrap;
}

function renderMountGuide() {
  const guide = $("mount-guide");
  const missing = (state.report?.packages ?? []).find((f) => f.missing_mount);
  guide.hidden = !missing;
  if (!missing) return;
  const prefix = missing.missing_mount;
  const bootstrap = "~/github/younsl/addons/box/addons/tether/scripts/bootstrap/bootstrap-dotfiles.sh";

  const title = document.createElement("h3");
  title.textContent = "The Brewfile is not being refreshed";
  const lead = document.createElement("p");
  lead.textContent = `tether cannot see ${prefix} inside its container, so it leaves the Brewfile as it is. On macOS, Podman runs containers in a VM that shares only some folders. Share ${prefix} with the VM, then start tether again.`;

  const steps = document.createElement("ol");
  const step1 = document.createElement("li");
  const s1 = document.createElement("p");
  s1.textContent = "Recreate the Podman VM with the folder shared. This deletes the images and containers in the VM.";
  step1.append(s1, codeBlock([
    "podman machine stop",
    "podman machine rm -f",
    "podman machine init --volume /Users:/Users --volume /private:/private \\",
    `  --volume /var/folders:/var/folders --volume ${prefix}:${prefix}`,
    "podman machine start",
  ].join("\n")));
  const step2 = document.createElement("li");
  const s2 = document.createElement("p");
  s2.textContent = `Start tether again. The bootstrap script mounts ${prefix} once the VM shares it.`;
  step2.append(s2, codeBlock(bootstrap));
  steps.append(step1, step2);

  guide.replaceChildren(title, lead, steps);
}

let renderedAt = null;

function render(ok) {
  renderHealth(ok);
  renderStatus();
  const at = state.report?.started_at ?? null;
  if (at === renderedAt && at !== null) return;
  renderedAt = at;
  renderFilters();
  renderRows();
  renderPackages();
  renderMountGuide();
}

async function refresh() {
  try {
    if (!state.info) {
      state.info = await getJson("/api/info");
      renderBuild();
    }
    const res = await fetch("/api/status", { cache: "no-store" });
    if (res.ok) state.report = await res.json();
    state.lastOk = Date.now();
    render(true);
  } catch {
    render(false);
  }
}

async function reconcileNow() {
  if (state.busy) return;
  const btn = $("reconcile");
  const before = state.report?.started_at;
  state.busy = true;
  btn.setAttribute("aria-busy", "true");
  const spinner = document.createElement("span");
  spinner.className = "spinner";
  spinner.setAttribute("aria-hidden", "true");
  btn.replaceChildren(spinner, document.createTextNode("Reconciling"));
  renderStatus();
  try {
    const res = await fetch("/api/reconcile", { method: "POST" });
    if (res.status !== 202) throw new Error(`Reconcile request returned ${res.status}.`);
    const deadline = Date.now() + 15000;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 400));
      const report = await getJson("/api/status").catch(() => null);
      if (report && report.started_at !== before) {
        state.report = report;
        break;
      }
    }
    if (state.report?.started_at === before) throw new Error("Reconcile did not finish within 15 seconds.");
    const r = state.report;
    const failed = r.entries.filter((e) => e.error).length;
    const changed = r.entries.filter((e) => e.applied).length;
    if (r.error) toast("Reconcile failed: the config file could not be loaded.", "error");
    else if (failed > 0) toast(`Reconciled. ${plural(failed, "link", "links")} failed.`, "error");
    else toast(changed > 0 ? `Reconciled. ${plural(changed, "link", "links")} updated.` : "Reconciled. Everything was already in place.");
  } catch (err) {
    toast(err.message, "error");
  } finally {
    state.busy = false;
    btn.removeAttribute("aria-busy");
    btn.replaceChildren(document.createTextNode("Reconcile now"));
    render(true);
  }
}

document.addEventListener("DOMContentLoaded", () => {
  $("reconcile").addEventListener("click", reconcileNow);
  $("search").addEventListener("input", (e) => {
    state.query = e.target.value;
    renderRows();
  });
  for (const el of document.querySelectorAll("[data-close]")) el.addEventListener("click", closeViewer);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      closeViewer();
      return;
    }
    if (!$("viewer").hidden || onLogsPage()) return;
    if (e.key === "/" && document.activeElement !== $("search")) {
      e.preventDefault();
      $("search").focus();
    }
  });
  for (const link of document.querySelectorAll(".nav-link")) {
    link.addEventListener("click", (e) => {
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      navigate(link.dataset.route);
    });
  }
  window.addEventListener("popstate", route);
  route();
  $("logs-follow").addEventListener("change", () => renderLogLines(true));
  refresh();
  pollLogs();
  setInterval(refresh, POLL_MS);
  setInterval(pollLogs, 2000);
});
