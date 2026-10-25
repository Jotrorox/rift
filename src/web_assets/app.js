(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    token: "", revision: null, baseline: "", writable: false,
    loaded: false, busy: false, polling: false, fetchingSource: false,
    session: 0, mutation: 0, remoteChanged: false, conflicted: false, online: false,
    serverButtons: [], view: "overview",
    permissions: null, groups: null, logServer: "", logCursor: null, logPolling: false, definitions: {}, definitionRevision: null, definitionDirty: false, definitionVersion: 0, definitionSection: "service_groups", definitionName: "", definitionInputs: new Map(),
    // Unsaved edits on the server list, tab list and permission group forms.
    settingsDirty: { serverList: false, tabList: false, permissionGroup: false }, permissionGroup: "", members: [], memberButtons: [],
    // Template downloads and the file browser.
    templateButtons: [], files: { root: "templates", path: "", editing: null, baseline: "", buttons: [] },
  };
  const settingsDirty = () => Object.values(state.settingsDirty).some(Boolean);
  const allowed = (permission) => state.permissions === null || state.permissions.includes(permission);
  const dirty = () => state.loaded && $("source").value !== state.baseline;
  const number = (value) => Number.isFinite(Number(value)) ? Number(value) : 0;
  const count = (value) => number(value).toLocaleString();
  const text = (id, value) => { $(id).textContent = value; };
  const bytes = (value) => {
    let amount = number(value), unit = 0;
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    while (amount >= 1024 && unit < units.length - 1) { amount /= 1024; unit += 1; }
    return `${amount.toFixed(unit && amount < 100 ? 1 : 0)} ${units[unit]}`;
  };
  const uptime = (value) => {
    const seconds = Math.floor(number(value));
    if (seconds >= 86400) return `${Math.floor(seconds / 86400)}d ${Math.floor(seconds % 86400 / 3600)}h`;
    if (seconds >= 3600) return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`;
    if (seconds >= 60) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
    return `${seconds}s`;
  };
  function notice(message, kind = "error") {
    text("notice", message);
    $("notice").className = message ? `notice ${kind}` : "notice hidden";
  }
  function result(message, kind = "") {
    text("config-result", message);
    $("config-result").className = `operation-result ${kind}`;
  }
  function connection(online) {
    state.online = online;
    text("connection-state", online ? "Connected" : "Unavailable");
    $("connection-light").className = `indicator ${online ? "good" : "bad"}`;
  }
  function controls() {
    const available = state.loaded && !state.busy;
    $("save").disabled = !available || !state.writable || !dirty() || state.remoteChanged || !allowed("config");
    $("validate").disabled = !available;
    $("refresh-source").disabled = state.busy;
    $("reload").disabled = !available;
    $("connect").disabled = state.busy;
    $("forget-token").disabled = state.busy;
    $("source").disabled = !state.loaded;
    text("edit-state", dirty() ? "Unsaved changes" : state.loaded ? state.writable ? "In sync" : "Read-only source" : "Not loaded");
    $("edit-state").className = `tag ${dirty() ? "warning" : state.loaded ? "good" : ""}`;
    text("source-size", `${$("source").value.split("\n").length.toLocaleString()} lines`);
    $("conflict").classList.toggle("hidden", !state.remoteChanged);
    for (const { button, unavailable } of state.serverButtons) button.disabled = state.busy || unavailable || !allowed("servers");
    $("console-send").disabled = state.busy || !state.logServer || !allowed("console");
    $("definition-save").disabled = state.busy || !state.definitionRevision || !allowed("config");
    $("definition-delete").disabled = state.busy || !state.definitionRevision || !$("definition-select").value || !allowed("config");
    const groupsEnabled = Boolean(state.definitions.permission_groups);
    for (const id of ["server-list-save", "tab-list-save", "permission-group-save"]) $(id).disabled = state.busy || !state.definitionRevision || !allowed("config");
    $("permission-group-delete").disabled = state.busy || !state.definitionRevision || !$("permission-group-select").value || !allowed("config");
    for (const button of state.memberButtons) button.disabled = state.busy || !allowed("config");
    // Fields that only matter while their setting is on.
    for (const id of ["server-list-motd", "server-list-maintenance-motd", "server-list-version", "server-list-max-players", "server-list-sample"]) $(id).disabled = !$("server-list-enabled").checked;
    for (const id of ["tab-list-header", "tab-list-footer", "tab-list-players"]) $(id).disabled = !$("tab-list-enabled").checked;
    for (const row of $("permission-groups").children) if (row.dataset) row.setAttribute("aria-current", String(row.dataset.group === state.permissionGroup && Boolean(state.permissionGroup)));
    $("permission-member-add").disabled = state.busy || !groupsEnabled || !$("permission-member-group").value || !allowed("config");
    for (const [id, permission] of [["config-panel", "config"], ["definitions-panel", "config"], ["server-list-panel", "config"], ["server-list-lua-panel", "config"], ["tab-list-panel", "config"], ["deployments-panel", "deploy"], ["audit-panel", "audit"], ["extensions-panel", "extensions"]]) $(id).classList.toggle("hidden", !allowed(permission));
    for (const id of ["permission-groups-panel", "permission-members-panel"]) $(id).classList.toggle("hidden", !allowed("config") || !groupsEnabled);
    $("permissions-unavailable").classList.toggle("hidden", !allowed("config") || groupsEnabled || !state.definitionRevision);
    $("live-console-panel").classList.toggle("hidden", !allowed("logs") && !allowed("console"));
    for (const { button, unavailable } of state.templateButtons) button.disabled = state.busy || unavailable || !allowed("servers");
    const filesDirty = state.files.editing !== null && $("files-content").value !== state.files.baseline;
    for (const id of ["files-open", "files-up", "files-new-file", "files-new-folder", "files-upload", "files-refresh"]) $(id).disabled = state.busy || !allowed("files");
    $("files-save").disabled = state.busy || !allowed("files") || state.files.editing === null || !filesDirty;
    for (const button of state.files.buttons) button.disabled = state.busy || !allowed("files");
    $("files-editor").classList.toggle("hidden", state.files.editing === null);
    text("files-edit-state", filesDirty ? "Unsaved changes" : "Saved");
    $("files-edit-state").className = `tag ${filesDirty ? "warning" : "good"}`;
    const visible = { script: allowed("config"), definitions: allowed("config"), extensions: allowed("extensions"),
      "server-list": allowed("config"), "tab-list": allowed("config"), permissions: allowed("config"),
      console: allowed("logs") || allowed("console"), deployments: allowed("deploy"), audit: allowed("audit"), files: allowed("files") };
    for (const [id] of views) $(`nav-${id}`).classList.toggle("hidden", visible[id] === false);
    for (const [section, ids] of sections) $(`section-${section}`).classList.toggle("hidden", ids.every((id) => visible[id] === false));
    showView();
  }
  const views = [["overview", "Overview"], ["routing", "Routing"], ["server-list", "Server list"], ["tab-list", "Tab list"], ["metrics", "Metrics"], ["servers", "Servers"], ["console", "Console"], ["groups", "Service groups"], ["files", "Files"],
    ["script", "Lua script"], ["definitions", "Groups & templates"], ["permissions", "Permissions"], ["extensions", "Extensions"], ["reference", "Reference"], ["deployments", "Deployments"], ["audit", "Audit log"]];
  const sections = [["configuration", ["script", "definitions", "permissions", "extensions"]], ["activity", ["deployments", "audit"]]];
  function showView() {
    const available = views.filter(([id]) => !$(`nav-${id}`).className.includes("hidden"));
    const requested = String(window.location.hash || "").slice(1);
    // Other fragments, such as the skip link, keep the current view.
    if (views.some(([id]) => id === requested)) state.view = requested;
    const [current, label] = available.find(([id]) => id === state.view) || available[0];
    const changed = $(`view-${current}`).hidden !== false;
    for (const [id] of views) {
      $(`view-${id}`).hidden = id !== current;
      $(`nav-${id}`).children[0]?.setAttribute("aria-current", id === current ? "page" : "false");
    }
    text("view-title", label);
    document.title = `${label} · Rift`;
    // The audit log is fetched on demand rather than on every poll.
    if (changed && current === "audit" && allowed("audit")) loadAudit().catch((error) => text("audit-result", error.message));
    if (changed && current === "permissions" && allowed("config") && state.definitions.permission_groups) loadPermissions().catch((error) => text("permission-member-result", error.message));
    if (changed && current === "groups" && allowed("read")) loadTemplates().catch((error) => text("template-result", error.message));
    if (changed && current === "files" && allowed("files")) loadFiles().catch((error) => text("files-result", error.message));
  }
  function sidebar(open) {
    $("app").classList.toggle("nav-open", open);
    $("sidebar-toggle").setAttribute("aria-expanded", String(open));
    if (open) document.querySelector?.(".nav a[aria-current=page]")?.focus();
  }
  window.addEventListener("hashchange", () => { sidebar(false); showView(); window.scrollTo?.(0, 0); });
  $("sidebar-toggle").addEventListener("click", () => sidebar(!$("app").className.includes("nav-open")));
  $("sidebar-scrim").addEventListener("click", () => sidebar(false));
  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !$("app").className.includes("nav-open")) return;
    sidebar(false); $("sidebar-toggle").focus();
  });
  async function request(path, options = {}, raw = false) {
    const headers = new Headers(options.headers || {});
    headers.set("Accept", raw ? "*/*" : "application/json");
    if (state.token) headers.set("Authorization", `Bearer ${state.token}`);
    if (options.body !== undefined && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
    const abort = new AbortController();
    const timeout = setTimeout(() => abort.abort(), 15000);
    try {
      const response = await fetch(path, { ...options, headers, signal: abort.signal, cache: "no-store", credentials: "omit", redirect: "error", mode: "same-origin" });
      const body = await response.text();
      if (raw) return { response, body };
      let data;
      try { data = body ? JSON.parse(body) : {}; } catch (_) { data = {}; }
      if (!response.ok) {
        const fallback = response.status === 401 || response.status === 403
          ? "Access denied. Enter the configured admin token under API access."
          : response.status === 404
            ? "The admin API is unavailable. Enable web.api in the Lua configuration, or check that this is the admin service address."
            : `Request failed (HTTP ${response.status}).`;
        const error = new Error(response.status === 404 && !path.startsWith("/api/operations/") ? fallback : data.error || fallback);
        error.status = response.status;
        throw error;
      }
      if (!response.headers.get("content-type")?.includes("application/json")) throw new Error("The server returned an unexpected response instead of JSON.");
      return data;
    } catch (error) {
      if (error.name === "AbortError") throw new Error("Request timed out. Check the service address before retrying.");
      throw error;
    } finally { clearTimeout(timeout); }
  }
  function handleError(error, editor = false) {
    const message = error.message || "The request could not be completed.";
    if (error.status === 401 || error.status === 403) $("auth-panel").open = true;
    if (error.status === 409) { state.remoteChanged = true; state.conflicted = true; }
    if (editor) result(message, "error"); else notice(message);
    controls();
  }
  function table(id, values, width, row, empty = "None configured") {
    const body = $(id);
    body.replaceChildren();
    for (const value of values) {
      const tr = document.createElement("tr");
      row(tr, value);
      body.append(tr);
    }
    if (!values.length) {
      const tr = document.createElement("tr"), td = document.createElement("td");
      td.colSpan = width; td.className = "empty"; td.textContent = empty;
      tr.append(td); body.append(tr);
    }
  }
  function cell(row, value, className = "") {
    const td = document.createElement("td");
    td.textContent = value ?? "—"; td.className = className; row.append(td);
    return td;
  }
  function renderStatus(status) {
    const metrics = status.metrics || {}, listeners = status.listeners || [], backends = status.backends || [];
    const checks = status.health_checks_enabled !== false;
    const healthy = checks ? backends.filter((backend) => backend.healthy === true).length : 0;
    const unknown = checks ? backends.filter((backend) => typeof backend.healthy !== "boolean").length : backends.length;
    text("version", status.version ? `v${status.version}` : "");
    text("metric-active", count(metrics.active));
    text("metric-accepted", `${count(metrics.accepted)} accepted since startup`);
    text("metric-backends", unknown === backends.length && backends.length ? "Unmonitored" : `${healthy} / ${backends.length}`);
    text("metric-health", unknown ? `${unknown} without a health result` : "Healthy backends / total");
    text("metric-traffic", bytes(number(metrics.sent) + number(metrics.received)));
    text("metric-directions", `${bytes(metrics.sent)} sent · ${bytes(metrics.received)} received`);
    text("metric-uptime", uptime(status.uptime_seconds));
    text("metric-errors", `${count(metrics.errors)} errors · ${count(metrics.reloads)} reloads`);
    text("listener-count", `${listeners.length} configured`);
    text("backend-count", `${backends.length} configured`);
    text("last-update", `Updated ${new Date().toLocaleTimeString()}`);
    if (status.config_path) text("config-path", status.config_path);
    table("listeners", listeners, 2, (tr, listener) => { cell(tr, listener.name); cell(tr, listener.address, "address"); });
    table("backends", backends, 3, (tr, backend) => {
      cell(tr, backend.name); cell(tr, backend.address, "address");
      const td = cell(tr, ""), span = document.createElement("span");
      span.className = `health ${!checks ? "unknown" : backend.healthy === false ? "bad" : backend.healthy === true ? "" : "unknown"}`;
      span.textContent = !checks ? "Checks disabled" : backend.healthy === false ? "Unhealthy" : backend.healthy === true ? "Healthy" : "Unknown";
      td.append(span);
    });
    state.serverButtons = [];
    table("service-groups", status.service_groups || [], 6, (tr, group) => {
      cell(tr, group.name);
      cell(tr, (group.port_range || []).join("–"));
      cell(tr, group.storage === "disposable" ? "Disposable game" : "Persistent world");
      cell(tr, group.template || "Prepared directories");
      cell(tr, (group.instances || []).join(", ") || "None created");
      const actions = cell(tr, ""), button = document.createElement("button");
      button.type = "button"; button.className = "button secondary"; button.textContent = "Create instance";
      button.setAttribute("aria-label", `Create instance in ${group.name}`);
      const full = Boolean(group.scaling) && (group.instances || []).length >= number(group.scaling.max_instances);
      if (full) button.title = "This group has reached its scaling maximum. Remove an instance or raise max_instances first.";
      state.serverButtons.push({ button, unavailable: full });
      button.addEventListener("click", () => { if (!button.disabled) instanceOperation(group.name, "create", group.storage); });
      actions.append(button);
    });
    table("managed-servers", status.managed_servers || [], 9, (tr, server) => {
      cell(tr, server.name); cell(tr, server.state, `state ${server.state}`);
      cell(tr, `${count(server.players)} players · ${count(server.reservations)} attachments`);
      cell(tr, wakeLabel(server));
      cell(tr, server.storage === "disposable" ? "Disposable game" : "Persistent world");
      cell(tr, server.template || "Prepared directory");
      cell(tr, server.group ? `${server.group} · ${server.address}` : server.address, "address");
      cell(tr, server.last_error || "—");
      const actions = cell(tr, ""); actions.className = "action-group";
      for (const action of ["start", "stop"]) {
        const button = document.createElement("button");
        button.type = "button"; button.className = "button subtle";
        button.textContent = action === "start" ? "Start" : "Stop";
        button.setAttribute("aria-label", `${button.textContent} ${server.name}`);
        const unavailable = action === "start"
          ? ["starting", "running", "stopping", "removing"].includes(server.state)
          : number(server.players) > 0 || number(server.reservations) > 0 || ["stopping", "removing"].includes(server.state) || (server.state === "stopped" && !server.automatic_start);
        state.serverButtons.push({ button, unavailable });
        button.addEventListener("click", () => serverOperation(server.name, action));
        actions.append(button);
      }
      if (server.group) {
        const button = document.createElement("button");
        button.type = "button"; button.className = server.storage === "disposable" ? "button destructive" : "button subtle";
        button.textContent = server.storage === "disposable" ? "Remove & delete files" : "Remove · keep files";
        button.setAttribute("aria-label", `${button.textContent}: ${server.name}`);
        button.title = server.storage === "disposable" ? "Stops the instance and permanently deletes its generated directory, including the world." : "Stops and unregisters the instance; its world and server files remain on disk.";
        const unavailable = number(server.players) > 0 || number(server.reservations) > 0 || ["stopping", "removing"].includes(server.state);
        state.serverButtons.push({ button, unavailable });
        button.addEventListener("click", () => {
          if (button.disabled) return;
          if (server.storage === "disposable" && !window.confirm(`Remove ${server.name} and permanently delete its files, including its world? This can't be undone.`)) return;
          instanceOperation(server.name, "remove", server.storage);
        });
        actions.append(button);
      }
    }, "No managed servers yet. Create one from a service group.");
    const root = $("files-root").value || state.files.root;
    $("files-root").replaceChildren();
    for (const [value, label] of [["templates", "Templates"], ...(status.managed_servers || []).map((server) => [`server:${server.name}`, `Server · ${server.name}`])]) {
      const option = document.createElement("option"); option.value = value; option.textContent = label; $("files-root").append(option);
    }
    $("files-root").value = root === "templates" || (status.managed_servers || []).some((server) => `server:${server.name}` === root) ? root : "templates";
    const selected = $("console-server").value;
    $("console-server").replaceChildren();
    for (const server of [{ name: "", label: "Choose a server" }, ...(status.managed_servers || [])]) {
      const option = document.createElement("option"); option.value = server.name; option.textContent = server.label || server.name; $("console-server").append(option);
    }
    $("console-server").value = (status.managed_servers || []).some((s) => s.name === selected) ? selected : "";
    if (!$("console-server").value && state.logServer) { state.logServer = ""; state.logCursor = null; text("console-output", "Choose a server to view its log."); }
    const routes = Object.entries(status.routes || {}).flatMap(([listener, route]) => typeof route === "string"
      ? [[listener, "All traffic", route]]
      : Object.entries(route).map(([pattern, backend]) => [listener, pattern, backend]));
    table("routes", routes, 4, (tr, [listener, pattern, backend]) => {
      cell(tr, listener); cell(tr, pattern, "address"); cell(tr, backend);
      cell(tr, (status.fallbacks?.[backend] || []).join(" → ") || "—", "address");
    });
    table("metrics", Object.entries(metrics).filter(([, value]) => typeof value === "number"), 2, (tr, [key, value]) => {
      cell(tr, key.replaceAll("_", " ")); cell(tr, count(value), "metric-value");
    });
    const services = status.services || {};
    $("services").replaceChildren();
    for (const [label, service] of [["Admin", services.web], ["Status", services.status], ["Metrics", services.metrics]]) {
      const dt = document.createElement("dt"), dd = document.createElement("dd");
      dt.textContent = label;
      dd.textContent = !service ? "Disabled" : typeof service === "string" ? service : `${service.listen} · ${Object.entries(service).filter(([key, value]) => typeof value === "boolean" && key !== "enabled").map(([key, value]) => `${key} ${value ? "on" : "off"}`).join(" · ")}`;
      $("services").append(dt, dd);
    }
    text("hook-state", status.http_hook ? "Hook enabled" : "No hook configured");
    $("hook-state").className = `tag ${status.http_hook ? "good" : ""}`;
    state.remoteChanged = state.conflicted || (state.loaded && typeof status.revision === "string" && status.revision !== state.revision);
    controls();
  }
  async function loadSource({ discard = false } = {}) {
    if (state.fetchingSource) return false;
    const session = state.session, previous = $("source").value;
    state.fetchingSource = true;
    try {
      const config = await request("/api/config");
      if (session !== state.session) return false;
      if (typeof config.source !== "string" || typeof config.revision !== "string") throw new Error("The server returned an invalid configuration response.");
      // Never overwrite typing that happened while the HTTP request was running.
      if ($("source").value !== previous || (!discard && dirty())) {
        state.remoteChanged = state.conflicted || (state.loaded && config.revision !== state.revision);
        return false;
      }
      $("source").value = config.source;
      state.baseline = config.source; state.revision = config.revision;
      state.writable = Boolean(config.writable); state.loaded = true; state.remoteChanged = false; state.conflicted = false;
      text("revision", config.revision.slice(0, 16)); $("revision").title = config.revision;
      if (!state.writable) result("This instance has no writable configuration file. You can inspect and validate source; saving requires a file-backed configuration.", "warning");
      return true;
    } finally { state.fetchingSource = false; controls(); }
  }
  async function refreshStatus({ showError = true, syncSource = true } = {}) {
    if (state.polling) return;
    state.polling = true;
    const session = state.session, mutation = state.mutation;
    try {
      const status = await request("/api/status");
      if (session !== state.session || mutation !== state.mutation) return;
      renderStatus(status); connection(true); notice("");
      if (syncSource && state.remoteChanged && !dirty() && !state.busy) await loadSource();
    } catch (error) {
      if (session !== state.session) return;
      connection(false);
      if (showError) handleError(error);
    } finally { state.polling = false; }
  }
  async function connect() {
    if (state.busy) return;
    state.busy = true; controls();
    try {
      const access = await request("/api/access");
      state.permissions = Array.isArray(access.permissions) ? access.permissions : null;
      state.groups = access.groups;
      text("operator-name", access.name || "Local operator");
      if (allowed("read")) { await refreshStatus(); await loadTemplates().catch((error) => text("template-result", error.message)); }
      if (allowed("config")) await loadSource();
      else { $("source").value = ""; state.baseline = ""; state.revision = null; state.loaded = false; }
      if (allowed("config")) { await loadDefinitions(); if (state.definitions.permission_groups) await loadPermissions(); }
      if (allowed("deploy")) await loadDeployments();
    } catch (error) { handleError(error); }
    finally { state.busy = false; controls(); }
  }
  const time = (value) => new Date(number(value)).toLocaleString();
  async function pollLogs({ reset = false } = {}) {
    if (state.logPolling || !state.logServer || !allowed("logs")) return;
    const session = state.session, server = state.logServer;
    if (reset) state.logCursor = null;
    state.logPolling = true;
    try {
      const suffix = state.logCursor === null ? "" : `?cursor=${state.logCursor}`;
      const data = await request(`/api/servers/${encodeURIComponent(server)}/logs${suffix}`);
      if (session !== state.session || server !== state.logServer) return;
      const previous = reset || state.logCursor === null || data.truncated ? "" : $("console-output").textContent;
      text("console-output", (previous + (data.text || "")).slice(-128 * 1024));
      state.logCursor = data.cursor;
      if ($("console-follow").checked) $("console-output").scrollTop = $("console-output").scrollHeight;
    } catch (error) { if (session === state.session && server === state.logServer) text("console-result", error.message); }
    finally { state.logPolling = false; }
  }
  const groupFields = [
    ["directory", "Instance directory (optional; {name} required)", "text"], ["command", "Command arguments (one per line; optional with a template)", "array"],
    ["memory_mb", "Memory in MiB (default command only)", "number"],
    ["port_range.0", "First port", "number"], ["port_range.1", "Last port", "number"],
    ["template", "Template (optional)", "text"], ["storage", "Storage", ["persistent", "disposable"]],
    ["autostart", "Start new instances automatically", "boolean"], ["start_on_connect", "Start on player connection", "boolean"],
    ["idle_timeout_ms", "Idle timeout (ms; 0 disables)", "number"], ["start_timeout_ms", "Startup timeout (ms)", "number"],
    ["stop_timeout_ms", "Stop timeout (ms)", "number"], ["restart_delay_ms", "Recovery delay (ms)", "number"], ["restart_retries", "Recovery attempts", "number"],
    ["scaling_enabled", "Automatic scaling", "boolean"],
    ...[["min_instances", "Minimum instances"], ["max_instances", "Maximum instances"], ["spare_instances", "Spare instances"], ["capacity_per_instance", "Players per instance"], ["target_occupancy_percent", "Target occupancy (%)"], ["queue_threshold", "Queue threshold"], ["cooldown_ms", "Scaling cooldown (ms)"]].map(([key, label]) => [`scaling.${key}`, label, "number"]),
  ];
  const softwareTypes = ["", "paper", "folia", "purpur", "vanilla", "fabric", "url"];
  const templateFields = [
    ["software.type", "Server software", softwareTypes], ["software.version", "Version (latest or a release such as 1.21.8)", "text"],
    ["software.url", "Download URL (type url)", "text"], ["software.sha256", "SHA-256 (type url; optional)", "text"],
    ["server_jar", "Local server jar path (without software)", "text"], ["plugins", "Plugin paths (one per line)", "array"],
    ["configs", "Template folders, overlaid in order (one per line)", "array"], ["map", "Map directory (optional)", "text"],
    ["accept_eula", "Accept the Minecraft EULA for new instances", "boolean"],
  ];
  function renderDefinition() {
    const section = $("definition-section").value || "service_groups", name = $("definition-select").value;
    state.definitionSection = section; state.definitionName = name;
    const maximumName = section === "templates" ? 128 : 107;
    $("definition-name").maxLength = maximumName; $("definition-name").pattern = `[A-Za-z0-9_\\-]{1,${maximumName}}`;
    const value = state.definitions[section]?.[name] || (section === "service_groups" ? { storage: "persistent", start_on_connect: true, start_timeout_ms: 120000, stop_timeout_ms: 30000, restart_delay_ms: 5000, restart_retries: 3, idle_timeout_ms: 0 } : {});
    $("definition-name").value = name; $("definition-fields").replaceChildren(); state.definitionInputs = new Map();
    for (const [key, label, type] of section === "templates" ? templateFields : groupFields) {
      const wrapper = document.createElement("div"), title = document.createElement("label"); wrapper.className = "field";
      const input = document.createElement(Array.isArray(type) || type === "boolean" ? "select" : type === "array" ? "textarea" : "input");
      input.id = `definition-field-${key.replaceAll(".", "-")}`; title.setAttribute("for", input.id); title.textContent = label;
      let current = key === "scaling_enabled" ? Boolean(value.scaling) : key.split(".").reduce((v, k) => v?.[k], value);
      if (Array.isArray(type) || type === "boolean") {
        for (const item of type === "boolean" ? ["false", "true"] : type) { const option = document.createElement("option"); option.value = item; option.textContent = item === "true" ? "Enabled" : item === "false" ? "Disabled" : item || "None (local jar)"; input.append(option); }
        input.value = String(current ?? (type === "boolean" ? false : type[0]));
      } else { if (type !== "array") input.type = type === "number" ? "number" : "text"; if (type === "number") { input.min = "0"; input.step = "1"; } input.value = type === "array" ? [current || []].flat().join("\n") : current ?? ""; }
      if (key === "software.version") input.setAttribute("list", "software-versions");
      if (key === "software.type") input.addEventListener("change", () => loadVersions(input.value));
      input.addEventListener("input", () => { state.definitionDirty = true; state.definitionVersion += 1; });
      state.definitionInputs.set(key, { input, type }); wrapper.append(title, input); $("definition-fields").append(wrapper);
    }
    state.definitionDirty = false; controls();
    if (section === "templates") loadVersions(value.software?.type || "");
  }
  // Suggest releases for the chosen software; typing any version still works.
  async function loadVersions(type) {
    const list = $("software-versions"); list.replaceChildren();
    if (!type || type === "url" || !allowed("servers")) return;
    try {
      const data = await request(`/api/software/${encodeURIComponent(type)}/versions`);
      for (const version of ["latest", ...(data.versions || [])]) { const option = document.createElement("option"); option.value = version; list.append(option); }
    } catch (_) { /* Suggestions are optional; upstream may be unreachable. */ }
  }
  async function loadDefinitions() {
    const session = state.session, version = state.definitionVersion;
    const value = await request("/api/definitions");
    if (session !== state.session) return;
    if (version !== state.definitionVersion) { text("definition-result", "Your newer field edits were preserved. Refresh again when ready."); return false; }
    state.definitions = value; state.definitionRevision = value.revision || null;
    const select = $("definition-select"), previous = select.value;
    select.replaceChildren();
    for (const name of ["", ...Object.keys(value[$("definition-section").value || "service_groups"] || {})]) {
      const option = document.createElement("option"); option.value = name; option.textContent = name || "New definition"; select.append(option);
    }
    select.value = Object.hasOwn(value[$("definition-section").value || "service_groups"] || {}, previous) ? previous : "";
    renderDefinition();
    renderSettings();
  }
  function definitionValue() {
    const value = {}, scaling = {}, software = {}, ports = [], scalingEnabled = state.definitionInputs.get("scaling_enabled")?.input.value === "true";
    const softwareEnabled = Boolean(state.definitionInputs.get("software.type")?.input.value);
    for (const [key, { input, type }] of state.definitionInputs) {
      if (key === "scaling_enabled") continue;
      const raw = String(input.value);
      if (key.startsWith("scaling.") && !scalingEnabled) continue;
      if (key.startsWith("software.") && !softwareEnabled) continue;
      if (raw === "") continue;
      let parsed = type === "array" ? raw.split("\n").filter((line) => line.trim()) : type === "number" ? Number(raw) : type === "boolean" ? raw === "true" : raw;
      if (type === "number" && (!Number.isSafeInteger(parsed) || parsed < 0)) throw new Error("Numeric fields must be nonnegative integers.");
      if (key.startsWith("scaling.")) scaling[key.split(".")[1]] = parsed;
      else if (key.startsWith("software.")) software[key.split(".")[1]] = parsed;
      else if (key.startsWith("port_range.")) ports[Number(key.split(".")[1])] = parsed;
      else value[key] = parsed;
    }
    if (scalingEnabled) value.scaling = scaling;
    if (softwareEnabled) value.software = software;
    if ($("definition-section").value !== "templates") value.port_range = ports;
    return value;
  }
  async function saveDefinition(remove = false) {
    if (state.busy || !state.definitionRevision || !allowed("config")) return;
    if (dirty()) { text("definition-result", "Save or discard your Lua editor changes before editing a definition."); return; }
    const section = $("definition-section").value || "service_groups", name = $("definition-name").value;
    if (!/^[A-Za-z0-9_-]+$/.test(name) || name.length > (section === "templates" ? 128 : 107)) { text("definition-result", "Enter a name using letters, digits, underscores or hyphens."); return; }
    if (remove && !window.confirm(`Delete ${name} from configuration? Referenced definitions cannot be deleted.`)) return;
    const version = state.definitionVersion;
    state.busy = true; controls(); text("definition-result", "Validating and applying definition…");
    try {
      const saved = await request(`/api/definitions/${section}/${encodeURIComponent(name)}`, { method: "PUT", body: JSON.stringify({ revision: state.definitionRevision, definition: remove ? null : definitionValue() }) });
      state.mutation += 1; state.definitionRevision = saved.revision;
      if (version === state.definitionVersion) state.definitionDirty = false;
      await loadSource();
      if (version === state.definitionVersion) await loadDefinitions();
      await refreshStatus({ syncSource: false });
      if (allowed("deploy")) await loadDeployments();
      text("definition-result", `${remove ? "Definition deleted." : "Definition saved and applied."}${state.definitionDirty ? " Your newer field edits remain unsaved." : ""}`);
    } catch (error) { text("definition-result", error.status === 409 ? `${error.message} Refresh definitions after preserving your edits.` : error.message); }
    finally { state.busy = false; controls(); }
  }
  // Text fields take plain text or a JSON chat component. Text that merely
  // starts with a brace, such as "{player} joined", stays plain.
  const componentText = (value) => value === null || value === undefined ? "" : typeof value === "string" ? value : JSON.stringify(value, null, 2);
  function componentValue(raw) {
    if (!raw.trim()) return undefined;
    if (/^\s*[[{]/.test(raw)) {
      try { const parsed = JSON.parse(raw); if (parsed && typeof parsed === "object") return parsed; } catch (_) { /* Plain text. */ }
    }
    return raw;
  }
  const componentSummary = (value) => {
    const flatten = (part) => typeof part === "string" ? part : Array.isArray(part) ? part.map(flatten).join("") : part && typeof part === "object" ? `${part.text ?? ""}${(part.extra || []).map(flatten).join("")}` : "";
    return value === null || value === undefined ? "—" : flatten(value).replace(/§./g, "") || "Chat component";
  };
  const lines = (value) => value.split("\n").map((line) => line.trim()).filter(Boolean);
  function markSettings(key) { state.settingsDirty[key] = true; }
  // Re-render each settings form from the loaded definitions unless it has unsaved edits.
  function renderSettings() {
    if (!state.settingsDirty.serverList) renderServerList();
    if (!state.settingsDirty.tabList) renderTabList();
    renderPermissionGroups();
  }
  function renderServerList() {
    const list = state.definitions.server_list || null;
    $("server-list-enabled").checked = Boolean(list);
    $("server-list-motd").value = componentText(list?.motd);
    $("server-list-maintenance-motd").value = componentText(list?.maintenance_motd);
    $("server-list-version").value = list?.version ?? "";
    $("server-list-max-players").value = list?.max_players ?? "";
    $("server-list-sample").value = (list?.sample || []).join("\n");
    const favicon = typeof list?.favicon === "string" && list.favicon.startsWith("data:image/png;base64,") ? list.favicon : "";
    $("server-list-favicon").hidden = !favicon;
    if (favicon) $("server-list-favicon").src = favicon;
    text("server-list-favicon-state", favicon ? "Favicon configured in Lua" : "No favicon configured");
    table("server-list-hosts", Object.entries(list?.hosts || {}), 4, (tr, [host, entry]) => {
      cell(tr, host, "address"); cell(tr, componentSummary(entry.motd)); cell(tr, entry.version ?? "—"); cell(tr, entry.max_players ?? "—", "metric-value");
    }, "No hostname-specific entries.");
    state.settingsDirty.serverList = false;
  }
  function serverListValue() {
    if (!$("server-list-enabled").checked) return null;
    const value = {}, maximum = $("server-list-max-players").value.trim();
    for (const [key, id] of [["motd", "server-list-motd"], ["maintenance_motd", "server-list-maintenance-motd"]]) {
      const parsed = componentValue($(id).value);
      if (parsed !== undefined) value[key] = parsed;
    }
    if ($("server-list-version").value.trim()) value.version = $("server-list-version").value;
    if (maximum) {
      if (!/^\d+$/.test(maximum) || !Number.isSafeInteger(Number(maximum))) throw new Error("Maximum players must be a nonnegative integer.");
      value.max_players = Number(maximum);
    }
    const sample = $("server-list-sample").value.split("\n").filter((line) => line.trim());
    if (sample.length) value.sample = sample;
    return value;
  }
  function renderTabList() {
    const list = state.definitions.tab_list || null;
    $("tab-list-enabled").checked = Boolean(list);
    $("tab-list-header").value = componentText(list?.header);
    $("tab-list-footer").value = componentText(list?.footer);
    $("tab-list-players").value = list?.players === "network" ? "network" : "server";
    state.settingsDirty.tabList = false;
  }
  function tabListValue() {
    if (!$("tab-list-enabled").checked) return null;
    const value = {};
    for (const key of ["header", "footer"]) {
      const parsed = componentValue($(`tab-list-${key}`).value);
      if (parsed !== undefined) value[key] = parsed;
    }
    if ($("tab-list-players").value === "network") value.players = "network";
    if (!Object.keys(value).length) throw new Error("Set a header, a footer or list everyone on the network, or turn the tab list off.");
    return value;
  }
  function renderPermissionGroups() {
    const groups = state.definitions.permission_groups || {};
    const members = {};
    for (const member of state.members) for (const group of member.groups || []) members[group] = (members[group] || 0) + 1;
    const names = Object.keys(groups);
    text("permission-group-count", `${names.length} defined`);
    table("permission-groups", names, 5, (tr, name) => {
      const nodes = Object.values(groups[name].permissions || {});
      const granted = nodes.filter(Boolean).length;
      if (tr.dataset) tr.dataset.group = name;
      cell(tr, name, "address"); cell(tr, (groups[name].inherits || []).join(", ") || "—");
      cell(tr, nodes.length ? `${granted} granted · ${nodes.length - granted} denied` : "None");
      cell(tr, name === "default" ? "Everyone" : count(members[name] || 0), "metric-value");
      const actions = cell(tr, ""), button = document.createElement("button");
      button.type = "button"; button.className = "button subtle"; button.textContent = "Edit";
      button.setAttribute("aria-label", `Edit ${name}`);
      button.addEventListener("click", () => selectPermissionGroup(name));
      actions.append(button);
    }, "No groups defined yet. Create one below.");
    const select = $("permission-group-select"), previous = state.permissionGroup;
    select.replaceChildren();
    for (const name of ["", ...names]) {
      const option = document.createElement("option"); option.value = name; option.textContent = name || "New group"; select.append(option);
    }
    select.value = Object.hasOwn(groups, previous) ? previous : "";
    if (!state.settingsDirty.permissionGroup) renderPermissionGroup();
    const target = $("permission-member-group"), chosen = target.value;
    target.replaceChildren();
    // Everyone is in default implicitly, so it can't be assigned.
    for (const name of names.filter((name) => name !== "default")) {
      const option = document.createElement("option"); option.value = name; option.textContent = name; target.append(option);
    }
    target.value = names.includes(chosen) && chosen !== "default" ? chosen : names.find((name) => name !== "default") || "";
    controls();
  }
  function renderPermissionGroup() {
    const name = $("permission-group-select").value, group = state.definitions.permission_groups?.[name] || {};
    state.permissionGroup = name;
    $("permission-group-name").value = name;
    $("permission-group-inherits").value = (group.inherits || []).join("\n");
    $("permission-group-nodes").value = Object.entries(group.permissions || {}).map(([node, granted]) => `${granted ? "" : "-"}${node}`).join("\n");
    state.settingsDirty.permissionGroup = false;
  }
  function selectPermissionGroup(name) {
    if (state.settingsDirty.permissionGroup && !window.confirm("Discard unsaved group changes?")) return;
    $("permission-group-select").value = name; state.settingsDirty.permissionGroup = false; renderPermissionGroup(); controls();
    $("permission-group-name").focus?.();
  }
  function permissionGroupValue() {
    const permissions = {};
    for (const line of lines($("permission-group-nodes").value)) {
      const denied = line.startsWith("-"), node = denied ? line.slice(1).trim() : line;
      if (!/^(\*|[a-z0-9_.-]{1,64})$/.test(node.replace(/\.\*$/, "")) || node.startsWith(".")) throw new Error(`“${line}” isn’t a permission node, prefix.* wildcard or *.`);
      permissions[node] = !denied;
    }
    return { inherits: lines($("permission-group-inherits").value.replaceAll(",", "\n")), permissions };
  }
  async function loadPermissions() {
    const session = state.session, data = await request("/api/permissions");
    if (session !== state.session) return;
    state.members = Array.isArray(data.members) ? data.members : []; state.memberButtons = [];
    text("permission-member-count", `${state.members.length}${data.truncated ? "+" : ""} with groups`);
    table("permission-members", state.members, 3, (tr, member) => {
      cell(tr, member.name || "Offline", member.name ? "" : "unlisted"); cell(tr, member.uuid, "address");
      const groups = cell(tr, ""), list = document.createElement("div"); list.className = "chip-list";
      for (const group of member.groups || []) {
        const chip = document.createElement("span"), label = document.createElement("span"), remove = document.createElement("button");
        chip.className = "chip"; label.textContent = group;
        remove.type = "button"; remove.textContent = "×"; state.memberButtons.push(remove);
        remove.setAttribute("aria-label", `Remove ${member.name || member.uuid} from ${group}`); remove.title = `Remove from ${group}`;
        remove.addEventListener("click", () => changeMembership(member.uuid, group, false));
        chip.append(label, remove); list.append(chip);
      }
      groups.append(list);
    }, "No player belongs to a group yet. Everyone is in default when it exists.");
    if (data.truncated) text("permission-member-result", "Showing the first 1,000 members.");
    renderPermissionGroups();
  }
  async function changeMembership(player, group, add) {
    if (state.busy || !allowed("config")) return;
    state.busy = true; controls(); text("permission-member-result", add ? `Adding ${player} to ${group}…` : `Removing ${player} from ${group}…`);
    try {
      const changed = await request(`/api/permissions/members/${encodeURIComponent(player)}/${encodeURIComponent(group)}`, { method: add ? "POST" : "DELETE", body: "{}" });
      text("permission-member-result", changed.changed === false
        ? add ? `${player} was already in ${group}.` : `${player} wasn’t in ${group}.`
        : add ? `Added ${player} to ${group}. It applies to their next permission check.` : `Removed ${player} from ${group}.`);
      if (add && $("permission-member-player").value.trim() === player) $("permission-member-player").value = "";
      await loadPermissions();
    } catch (error) { if (error.status === 401 || error.status === 403) $("auth-panel").open = true; text("permission-member-result", error.message); }
    finally { state.busy = false; controls(); }
  }
  // Save one settings form through the structured definition endpoint.
  async function applySetting({ path, value, result, key, saved }) {
    if (state.busy || !state.definitionRevision || !allowed("config")) return;
    if (dirty()) { text(result, "Save or discard your Lua editor changes first."); return; }
    let definition;
    try { definition = value(); } catch (error) { text(result, error.message); return; }
    state.busy = true; controls(); text(result, "Validating and applying…");
    try {
      const response = await request(path, { method: "PUT", body: JSON.stringify({ revision: state.definitionRevision, definition }) });
      state.mutation += 1; state.definitionRevision = response.revision; state.settingsDirty[key] = false;
      await loadSource(); await loadDefinitions();
      await refreshStatus({ syncSource: false });
      if (allowed("deploy")) await loadDeployments();
      text(result, saved(definition));
    } catch (error) { text(result, error.status === 409 ? `${error.message} Your edits are kept; refresh, then reapply them.` : error.message); }
    finally { state.busy = false; controls(); }
  }
  async function refreshSettings(key, result) {
    if (state.busy || (state.settingsDirty[key] && !window.confirm("Discard unsaved changes and refresh?"))) return;
    state.settingsDirty[key] = false;
    try {
      await loadDefinitions();
      if (state.definitions.permission_groups) await loadPermissions();
      text(result, "Refreshed.");
    } catch (error) { text(result, error.message); }
  }
  async function loadDeployments() {
    const session = state.session, data = await request("/api/deployments");
    if (session !== state.session) return;
    state.deploymentRevision = data.revision;
    table("deployments", data.deployments || [], 5, (tr, deployment) => {
      cell(tr, time(deployment.timestamp_unix_ms)); cell(tr, deployment.actor); cell(tr, deployment.action); cell(tr, deployment.revision, "address");
      const actions = cell(tr, ""), button = document.createElement("button"); button.type = "button"; button.className = "button secondary"; button.textContent = "Roll back";
      button.addEventListener("click", async () => {
        if (state.busy || !allowed("deploy")) return;
        if (dirty() || state.definitionDirty || settingsDirty()) { text("deployment-result", "Save or discard editor changes before rolling back."); return; }
        if (!window.confirm(`Restore deployment ${deployment.id}? Configuration will be validated before applying.`)) return;
        state.busy = true; controls(); text("deployment-result", "Restoring deployment…");
        try {
          await request(`/api/deployments/${deployment.id}/rollback`, { method: "POST", body: JSON.stringify({ revision: state.deploymentRevision }) });
          state.mutation += 1;
          if (allowed("config")) { await loadSource(); await loadDefinitions(); }
          if (allowed("read")) await refreshStatus({ syncSource: false });
          await loadDeployments(); text("deployment-result", "Deployment restored; worlds and instance files are preserved.");
        } catch (error) { text("deployment-result", error.message); }
        finally { state.busy = false; controls(); }
      }); actions.append(button);
    }, "No deployments recorded yet.");
  }
  async function loadAudit() {
    const session = state.session, data = await request("/api/audit");
    if (session !== state.session) return;
    table("audit-records", data.records || [], 5, (tr, record) => { cell(tr, time(record.timestamp_unix_ms)); cell(tr, record.actor); cell(tr, record.action); cell(tr, record.target || "—"); cell(tr, `${record.outcome}${record.status ? ` (${record.status})` : ""}`); }, "No audit records yet.");
    text("audit-result", data.durable ? "Records are retained on disk." : "Records are held in memory for this process.");
  }
  function wakeLabel(server) {
    if (server.restart_exhausted) return "Paused · restart retries exhausted; Start to retry";
    if (server.automatic_enabled === false) return "Paused by Stop; Start to resume";
    return server.automatic_start ? "Enabled" : "Explicit start only";
  }
  function checkSource() {
    const source = $("source").value;
    if (new TextEncoder().encode(source).length > 256 * 1024) throw new Error("Lua source exceeds the 256 KiB limit.");
    return source;
  }
  async function validate() {
    if (!state.loaded || state.busy) return;
    state.busy = true; controls(); result("Validating configuration…");
    try {
      const source = checkSource();
      const validation = await request("/api/config/validate", { method: "POST", body: JSON.stringify({ source }) });
      if (validation.valid !== true) throw new Error(validation.error || "Configuration validation failed.");
      result($("source").value === source ? "Configuration is valid. No changes have been written or applied." : "The submitted configuration is valid. You have made further edits; validate again before saving.", "success");
    } catch (error) { handleError(error, true); }
    finally { state.busy = false; controls(); }
  }
  async function save() {
    if (!state.loaded || state.busy || !state.writable || !dirty() || state.remoteChanged) return;
    state.busy = true; controls(); result("Validating and saving configuration…");
    let submitted = false;
    try {
      const source = checkSource(), revision = state.revision;
      const validation = await request("/api/config/validate", { method: "POST", body: JSON.stringify({ source }) });
      if (validation.valid !== true) throw new Error(validation.error || "Configuration validation failed.");
      submitted = true;
      const saved = await request("/api/config", { method: "PUT", body: JSON.stringify({ source, revision }) });
      state.mutation += 1;
      state.baseline = source;
      if (typeof saved.revision === "string") state.revision = saved.revision;
      state.remoteChanged = false; state.conflicted = false;
      text("revision", state.revision.slice(0, 16)); $("revision").title = state.revision;
      result(`${saved.message || "Configuration saved and applied."}${dirty() ? " Your newer edits remain unsaved." : ""}`, "success");
      await refreshStatus({ syncSource: false });
    } catch (error) {
      handleError(error, true);
      if (submitted && !error.status) result(`${error.message} The save may have succeeded if it moved or disabled this service. Your editor contents are preserved; check the configured address and current source before retrying.`, "warning");
    } finally { state.busy = false; controls(); }
  }
  async function reload() {
    if (!state.loaded || state.busy) return;
    if (dirty() && !window.confirm("Reload the current file from disk into the running proxy? Your unsaved editor changes will be kept.")) return;
    state.busy = true; controls(); result("Reloading configuration from disk…");
    try {
      const reloaded = await request("/api/reload", { method: "POST" });
      state.mutation += 1;
      if (!dirty()) await loadSource();
      result(`${reloaded.message || "Configuration reloaded from disk."}${dirty() ? " Your unsaved editor changes were kept." : ""}`, "success");
      await refreshStatus({ syncSource: false });
    } catch (error) {
      handleError(error, true);
      if (!error.status) result(`${error.message} Reloading may have moved or disabled this service. Check the configured address.`, "warning");
    } finally { state.busy = false; controls(); }
  }
  async function serverOperation(name, action) {
    if (state.busy) return;
    state.busy = true; controls();
    text("server-result", `Requesting ${action} for ${name}…`);
    $("server-result").className = "operation-result";
    try {
      await request(`/api/servers/${encodeURIComponent(name)}/${action}`, { method: "POST", body: "{}" });
      text("server-result", `${action === "start" ? "Start" : "Stop"} accepted for ${name}. State updates below; ${action === "stop" ? "automatic wake stays paused until Start is requested." : "wait for running before connecting."}`);
      $("server-result").className = "operation-result success";
      await refreshStatus({ syncSource: false });
    } catch (error) {
      if (error.status === 401 || error.status === 403) $("auth-panel").open = true;
      text("server-result", error.message || "The server request failed.");
      $("server-result").className = "operation-result error";
    } finally { state.busy = false; controls(); }
  }
  async function instanceOperation(name, action, storage) {
    if (state.busy) return;
    state.busy = true; state.mutation += 1; controls();
    text("server-result", `${action === "create" ? "Creating an instance in" : "Removing"} ${name}…`);
    $("server-result").className = "operation-result";
    let operationId = "";
    try {
      let outcome = await request(action === "create"
        ? `/api/groups/${encodeURIComponent(name)}/instances`
        : `/api/instances/${encodeURIComponent(name)}`, { method: action === "create" ? "POST" : "DELETE", body: "{}" });
      if (outcome.operation_id) {
        operationId = outcome.operation_id;
        const path = `/api/operations/${encodeURIComponent(operationId)}`;
        text("server-result", `${action === "create" ? "Creation" : "Removal"} in progress for ${name}. Operation ${operationId}; waiting for completion…`);
        for (;;) {
          const operation = await request(path);
          if (operation.status === "succeeded") { outcome = operation.result; break; }
          if (operation.status === "failed") {
            const error = new Error(operation.error || "The instance operation failed.");
            error.status = operation.http_status;
            error.operationFailed = true;
            throw error;
          }
          if (operation.status !== "pending") throw new Error("The server returned an invalid operation status.");
          await new Promise((resolve) => setTimeout(resolve, 1000));
        }
      }
      const cleanupFailed = action === "remove" && Boolean(outcome.cleanup_error);
      text("server-result", action === "create"
        ? `Created ${outcome.name || "instance"} in ${name}. Check its state below; provisioned servers require your EULA acceptance and backend settings before starting.`
        : cleanupFailed ? `Removed ${name}, but file cleanup failed: ${outcome.cleanup_error}`
          : storage === "disposable"
            ? outcome.files_removed === true ? `Removed ${name} and deleted its instance files, including its world.` : `Removed ${name}. Instance files were not deleted; inspect the directory before reuse.`
            : `Removed ${name}. Its persistent world and server files remain on disk.`);
      $("server-result").className = `operation-result ${cleanupFailed || (action === "remove" && storage === "disposable" && outcome.files_removed !== true) ? "warning" : "success"}`;
      await refreshStatus({ syncSource: false });
    } catch (error) {
      if (error.status === 401 || error.status === 403) $("auth-panel").open = true;
      const message = error.message || "The instance request failed.";
      text("server-result", operationId
        ? error.operationFailed ? `Operation ${operationId} failed: ${message}`
          : `Could not retrieve operation ${operationId}: ${message} It may still be running; check /api/operations/${encodeURIComponent(operationId)} before retrying.`
        : message);
      $("server-result").className = "operation-result error";
    } finally { state.busy = false; controls(); }
  }
  $("source").addEventListener("input", controls);
  $("source").addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); validate(); }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") { event.preventDefault(); save(); }
  });
  window.addEventListener("beforeunload", (event) => {
    if (dirty() || state.definitionDirty || settingsDirty()) { event.preventDefault(); event.returnValue = ""; }
  });
  $("auth-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (state.busy) return;
    if (!resetOperatorView()) return;
    state.token = $("token").value.trim(); $("token").value = ""; state.session += 1;
    text("auth-state", state.token ? "Token held in this tab" : "No token set");
    connect();
  });
  $("forget-token").addEventListener("click", () => {
    if (state.busy) return;
    if (!resetOperatorView()) return;
    state.token = ""; $("token").value = ""; state.session += 1;
    text("auth-state", "No token set"); connect();
  });
  function resetOperatorView() {
    const filesDirty = state.files.editing !== null && $("files-content").value !== state.files.baseline;
    if ((dirty() || state.definitionDirty || settingsDirty() || filesDirty) && !window.confirm("Switch operator and discard unsaved editor changes?")) return false;
    state.loaded = false; state.baseline = ""; state.revision = null; state.remoteChanged = false; state.conflicted = false;
    state.permissions = []; state.logServer = ""; state.logCursor = null; state.definitions = {}; state.definitionRevision = null; state.definitionDirty = false; state.definitionVersion += 1;
    state.settingsDirty = { serverList: false, tabList: false, permissionGroup: false }; state.permissionGroup = ""; state.members = []; state.memberButtons = [];
    $("source").value = ""; $("console-server").replaceChildren(); $("definition-fields").replaceChildren();
    for (const id of ["console-output", "config-path", "revision", "deployment-result", "definition-result", "console-result", "extension-output"]) text(id, "");
    for (const id of ["deployments", "audit-records", "managed-servers", "service-groups", "backends", "routes", "server-list-hosts", "permission-groups", "permission-members", "templates", "files-entries"]) $(id).replaceChildren();
    state.templateButtons = []; state.files = { root: "templates", path: "", editing: null, baseline: "", buttons: [] };
    for (const id of ["template-result", "files-result"]) text(id, "");
    for (const id of ["server-list-result", "tab-list-result", "permission-group-result", "permission-member-result"]) text(id, "");
    state.serverButtons = []; return true;
  }
  $("refresh-status").addEventListener("click", () => refreshStatus());
  $("console-server").addEventListener("change", () => {
    state.logServer = $("console-server").value; state.logCursor = null;
    text("console-output", ""); text("console-result", ""); controls(); pollLogs({ reset: true });
  });
  $("console-refresh").addEventListener("click", () => pollLogs());
  $("console-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (state.busy || !state.logServer || !allowed("console")) return;
    const session = state.session, server = state.logServer, command = $("console-command").value;
    state.busy = true; controls();
    try {
      const data = await request(`/api/servers/${encodeURIComponent(server)}/console`, { method: "POST", body: JSON.stringify({ command }) });
      if (session !== state.session) return;
      if ($("console-command").value === command) $("console-command").value = "";
      text("console-result", data.message || "Command sent; check the log for its result."); await pollLogs();
    } catch (error) { if (session === state.session) text("console-result", error.message); }
    finally { state.busy = false; controls(); }
  });
  $("definition-form").addEventListener("submit", (event) => { event.preventDefault(); saveDefinition(); });
  $("definition-delete").addEventListener("click", () => saveDefinition(true));
  $("definition-name").addEventListener("input", () => { state.definitionDirty = true; state.definitionVersion += 1; });
  for (const id of ["definition-section", "definition-select"]) $(id).addEventListener("change", async () => {
    if (state.definitionDirty && !window.confirm("Discard unsaved definition fields?")) {
      $("definition-section").value = state.definitionSection; $("definition-select").value = state.definitionName;
      text("definition-result", "Your definition fields were kept. Refresh definitions to select another entry."); return;
    }
    if (id === "definition-section") { $("definition-select").value = ""; try { await loadDefinitions(); } catch (error) { text("definition-result", error.message); } }
    else renderDefinition();
  });
  $("definitions-refresh").addEventListener("click", async () => {
    if (state.busy || (state.definitionDirty && !window.confirm("Discard unsaved definition fields and refresh?"))) return;
    try { await loadDefinitions(); text("definition-result", "Definitions refreshed."); } catch (error) { text("definition-result", error.message); }
  });
  for (const [form, key] of [["server-list-form", "serverList"], ["tab-list-form", "tabList"], ["permission-group-form", "permissionGroup"]]) {
    $(form).addEventListener("input", () => markSettings(key));
    $(form).addEventListener("change", () => { markSettings(key); controls(); });
  }
  $("server-list-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (!$("server-list-enabled").checked && state.definitions.server_list && !window.confirm("Turn off the proxy-owned server list? Its favicon and hostname entries are removed too, and backends answer pings again.")) return;
    applySetting({ path: "/api/definitions/server_list", value: serverListValue, result: "server-list-result", key: "serverList",
      saved: (value) => value ? "Server list saved and applied." : "Server list turned off; backends answer pings again." });
  });
  $("tab-list-form").addEventListener("submit", (event) => {
    event.preventDefault();
    applySetting({ path: "/api/definitions/tab_list", value: tabListValue, result: "tab-list-result", key: "tabList",
      saved: (value) => value ? "Tab list saved; players see it within half a second." : "Tab list turned off; backends control it again." });
  });
  $("permission-group-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const name = $("permission-group-name").value.trim();
    if (!/^[a-z0-9_.-]{1,64}$/.test(name)) { text("permission-group-result", "Enter a group name using lowercase letters, digits, dots, underscores or hyphens."); return; }
    if ($("permission-group-select").value === "" && Object.hasOwn(state.definitions.permission_groups || {}, name) && !window.confirm(`Replace the existing group ${name}?`)) return;
    state.permissionGroup = name;
    applySetting({ path: `/api/definitions/permission_groups/${encodeURIComponent(name)}`, value: permissionGroupValue, result: "permission-group-result", key: "permissionGroup",
      saved: () => `Group ${name} saved and applied.` });
  });
  $("permission-group-delete").addEventListener("click", () => {
    const name = $("permission-group-select").value;
    if (!name || !window.confirm(`Delete the group ${name}? Players keep the membership until you remove it, but it grants nothing.`)) return;
    state.permissionGroup = "";
    applySetting({ path: `/api/definitions/permission_groups/${encodeURIComponent(name)}`, value: () => null, result: "permission-group-result", key: "permissionGroup",
      saved: () => `Group ${name} deleted.` });
  });
  $("permission-group-select").addEventListener("change", () => {
    const chosen = $("permission-group-select").value;
    $("permission-group-select").value = state.permissionGroup;
    selectPermissionGroup(chosen);
  });
  $("permission-member-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const raw = $("permission-member-player").value.trim(), compact = raw.replaceAll("-", "").toLowerCase();
    const player = /^[0-9a-f]{32}$/.test(compact) ? compact : raw;
    if (!/^[0-9a-f]{32}$/.test(player) && !/^[A-Za-z0-9_]{1,16}$/.test(player)) { text("permission-member-result", "Enter a player UUID or the name of an online player."); return; }
    changeMembership(player, $("permission-member-group").value, true);
  });
  $("server-list-refresh").addEventListener("click", () => refreshSettings("serverList", "server-list-result"));
  $("tab-list-refresh").addEventListener("click", () => refreshSettings("tabList", "tab-list-result"));
  $("permissions-refresh").addEventListener("click", () => refreshSettings("permissionGroup", "permission-group-result"));
  $("deployments-refresh").addEventListener("click", async () => { try { await loadDeployments(); } catch (error) { text("deployment-result", error.message); } });
  $("audit-refresh").addEventListener("click", async () => { try { await loadAudit(); } catch (error) { text("audit-result", error.message); } });
  $("validate").addEventListener("click", validate);
  $("save").addEventListener("click", save);
  $("reload").addEventListener("click", reload);
  $("refresh-source").addEventListener("click", async () => {
    if (state.busy) return;
    if (dirty() && !window.confirm("Discard unsaved editor changes and load the current configuration source?")) return;
    state.busy = true; controls();
    try {
      const loaded = await loadSource({ discard: true });
      result(loaded ? "Loaded the current configuration source." : "Your newer edits were preserved; source was not replaced.", loaded ? "success" : "warning");
    }
    catch (error) { handleError(error, true); }
    finally { state.busy = false; controls(); }
  });
  $("extension-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const path = $("extension-path").value.trim(), method = $("extension-method").value;
    // Restrict this tool to local extension paths so a token cannot be sent elsewhere.
    let url;
    try { url = new URL(path, window.location.origin); } catch (_) { text("extension-output", "Enter a valid /ext/ path."); return; }
    if (url.origin !== window.location.origin || !(url.pathname === "/ext" || url.pathname.startsWith("/ext/")) || !path.startsWith("/ext") || url.hash) {
      text("extension-output", "Only paths under /ext/ on this service are allowed. URL fragments are not supported."); return;
    }
    $("extension-send").disabled = true; text("extension-output", "Sending request…");
    try {
      const options = { method };
      if (method !== "GET" && $("extension-body").value) {
        options.body = $("extension-body").value;
        try { JSON.parse(options.body); options.headers = { "Content-Type": "application/json" }; }
        catch (_) { options.headers = { "Content-Type": "text/plain; charset=utf-8" }; }
      }
      const { response, body } = await request(url.pathname + url.search, options, true);
      text("extension-output", `HTTP ${response.status} ${response.statusText}\n${response.headers.get("content-type") || "No content type"}\n\n${body}`);
    } catch (error) { text("extension-output", error.message); }
    finally { $("extension-send").disabled = false; }
  });
  // Templates: software, download cache and the groups that use them.
  async function loadTemplates() {
    const session = state.session, data = await request("/api/templates");
    if (session !== state.session) return;
    const templates = data.templates || [];
    text("template-count", `${templates.length} defined`);
    state.templateButtons = [];
    table("templates", templates, 5, (tr, template) => {
      const software = template.software;
      cell(tr, template.name);
      cell(tr, software ? (software.type === "url" ? `URL · ${software.url || ""}` : `${software.type} ${software.version}`) : "Local server jar", software?.type === "url" ? "address" : "");
      cell(tr, !software ? "—" : software.cached
        ? `Downloaded${software.resolved_version ? ` · ${software.resolved_version}` : ""}${software.build ? ` (build ${software.build})` : ""}${software.size ? ` · ${bytes(software.size)}` : ""}`
        : "Not downloaded · fetched on first create");
      cell(tr, (template.groups || []).join(", ") || "Unused");
      const actions = cell(tr, "");
      if (!software) return;
      const button = document.createElement("button");
      button.type = "button"; button.className = "button secondary";
      button.textContent = software.cached ? "Update" : "Download";
      button.title = software.cached ? "Download the newest matching build again. Existing instances keep their jar." : "Download now so new instances start without waiting.";
      button.setAttribute("aria-label", `${button.textContent} ${template.name}`);
      state.templateButtons.push({ button, unavailable: false });
      button.addEventListener("click", () => { if (!button.disabled) downloadTemplate(template.name, Boolean(software.cached)); });
      actions.append(button);
    }, "No templates configured. Add one under Groups & templates.");
    controls();
  }
  async function downloadTemplate(name, refresh) {
    if (state.busy) return;
    state.busy = true; controls();
    text("template-result", `${refresh ? "Updating" : "Downloading"} the server software for ${name}…`);
    $("template-result").className = "operation-result";
    try {
      const accepted = await request(`/api/templates/${encodeURIComponent(name)}/download`, { method: "POST", body: JSON.stringify({ refresh }) });
      let operation;
      for (;;) {
        operation = await request(accepted.poll);
        if (operation.status !== "pending") break;
        await new Promise((resolve) => setTimeout(resolve, 1000));
      }
      if (operation.status !== "succeeded") throw new Error(operation.error || "The download failed.");
      const software = operation.result?.software || {};
      text("template-result", `Downloaded ${software.type || "server"} ${software.resolved_version || software.version || ""} for ${name}.`.replace(/\s+\./, "."));
      $("template-result").className = "operation-result success";
      await loadTemplates();
    } catch (error) {
      if (error.status === 401 || error.status === 403) $("auth-panel").open = true;
      text("template-result", error.message); $("template-result").className = "operation-result error";
    } finally { state.busy = false; controls(); }
  }

  // File browser for template folders and server directories.
  const filePath = (path) => path.split("/").filter(Boolean).map(encodeURIComponent).join("/");
  function fileUrl(path = state.files.path, root = state.files.root) {
    const base = root === "templates" ? "/api/files/templates" : `/api/files/servers/${encodeURIComponent(root.slice("server:".length))}`;
    const encoded = filePath(path);
    return encoded ? `${base}/${encoded}` : base;
  }
  const joinPath = (...parts) => parts.flatMap((part) => String(part).split("/")).filter(Boolean).join("/");
  function filesDirty() { return state.files.editing !== null && $("files-content").value !== state.files.baseline; }
  async function loadFiles(path = $("files-path").value, root = $("files-root").value || "templates") {
    if (filesDirty() && !window.confirm("Discard unsaved file changes?")) return;
    const session = state.session, clean = joinPath(path);
    const data = await request(fileUrl(clean, root));
    if (session !== state.session) return;
    state.files.root = root; state.files.path = clean; state.files.editing = null; state.files.buttons = [];
    $("files-path").value = clean; $("files-root").value = root;
    table("files-entries", data.entries || [], 4, (tr, entry) => {
      const name = cell(tr, ""), open = document.createElement("button");
      open.type = "button"; open.className = "text-link"; open.textContent = entry.type === "directory" ? `${entry.name}/` : entry.name;
      if (entry.type === "directory") open.addEventListener("click", () => loadFiles(joinPath(clean, entry.name), root).catch((error) => text("files-result", error.message)));
      else if (entry.type === "file") open.addEventListener("click", () => openFile(joinPath(clean, entry.name), entry.size));
      else open.disabled = true;
      name.append(open);
      cell(tr, entry.type === "file" ? bytes(entry.size) : "—", "metric-value");
      cell(tr, entry.modified ? new Date(entry.modified).toLocaleString() : "—");
      const actions = cell(tr, ""); actions.className = "action-group";
      if (entry.type === "file") {
        const download = document.createElement("button");
        download.type = "button"; download.className = "button subtle"; download.textContent = "Download";
        download.setAttribute("aria-label", `Download ${entry.name}`);
        download.addEventListener("click", () => downloadFile(joinPath(clean, entry.name)));
        state.files.buttons.push(download); actions.append(download);
      }
      const remove = document.createElement("button");
      remove.type = "button"; remove.className = "button destructive"; remove.textContent = "Delete";
      remove.setAttribute("aria-label", `Delete ${entry.name}`);
      remove.addEventListener("click", () => deleteFile(joinPath(clean, entry.name), entry.type === "directory"));
      state.files.buttons.push(remove); actions.append(remove);
    }, data.truncated ? "Too many entries to list." : "This folder is empty.");
    text("files-result", data.truncated ? "Showing the first 2,000 entries." : "");
    controls();
  }
  async function openFile(path, size) {
    if (filesDirty() && !window.confirm("Discard unsaved file changes?")) return;
    if (number(size) > 1024 * 1024) { text("files-result", "Files over 1 MiB open as downloads."); downloadFile(path); return; }
    try {
      const { response, body } = await request(fileUrl(path), {}, true);
      if (!response.ok) { let message = `Request failed (HTTP ${response.status}).`; try { message = JSON.parse(body).error || message; } catch (_) { /* Not JSON. */ } throw new Error(message); }
      if (body.includes("\u0000")) { text("files-result", "This looks like a binary file. Download it instead."); return; }
      state.files.editing = path; state.files.baseline = body;
      $("files-content").value = body; text("files-editing", path); text("files-result", "");
      controls(); $("files-content").focus?.();
    } catch (error) { text("files-result", error.message); }
  }
  async function writeFile(path, body, { directory = false } = {}) {
    return request(`${fileUrl(path)}${directory ? "?directory" : ""}`, { method: "PUT", body, headers: { "Content-Type": "application/octet-stream" } });
  }
  async function fileMutation(message, work) {
    if (state.busy || !allowed("files")) return;
    state.busy = true; controls(); text("files-result", message);
    try { text("files-result", await work()); }
    catch (error) { if (error.status === 401 || error.status === 403) $("auth-panel").open = true; text("files-result", error.message); }
    finally { state.busy = false; controls(); }
  }
  function downloadFile(path) {
    fileMutation(`Preparing ${path}…`, async () => {
      const headers = new Headers(); if (state.token) headers.set("Authorization", `Bearer ${state.token}`);
      const response = await fetch(fileUrl(path), { headers, cache: "no-store", credentials: "omit", redirect: "error", mode: "same-origin" });
      if (!response.ok) throw new Error(`Download failed (HTTP ${response.status}).`);
      const link = document.createElement("a"), url = URL.createObjectURL(await response.blob());
      link.href = url; link.download = path.split("/").pop(); link.click(); setTimeout(() => URL.revokeObjectURL(url), 1000);
      return `Downloaded ${path}.`;
    });
  }
  function deleteFile(path, directory) {
    if (!window.confirm(`Delete ${path}${directory ? " and everything inside it" : ""}? This can't be undone.`)) return;
    fileMutation(`Deleting ${path}…`, async () => {
      await request(fileUrl(path), { method: "DELETE" });
      if (state.files.editing === path) state.files.editing = null;
      state.busy = false; await loadFiles(state.files.path, state.files.root);
      return `Deleted ${path}.`;
    });
  }
  function newEntry(directory) {
    const name = $("files-new-name").value.trim();
    if (!name || name.includes("/") || name === "." || name === "..") { text("files-result", "Enter a file or folder name without slashes."); return; }
    const path = joinPath(state.files.path, name);
    fileMutation(`Creating ${path}…`, async () => {
      await writeFile(path, "", { directory });
      $("files-new-name").value = ""; state.busy = false;
      await loadFiles(state.files.path, state.files.root);
      if (!directory) await openFile(path, 0);
      return `Created ${path}.`;
    });
  }
  $("files-form").addEventListener("submit", (event) => { event.preventDefault(); loadFiles().catch((error) => text("files-result", error.message)); });
  $("files-refresh").addEventListener("click", () => loadFiles(state.files.path, state.files.root).catch((error) => text("files-result", error.message)));
  $("files-root").addEventListener("change", () => loadFiles("", $("files-root").value).catch((error) => text("files-result", error.message)));
  $("files-up").addEventListener("click", () => loadFiles(state.files.path.split("/").slice(0, -1).join("/"), state.files.root).catch((error) => text("files-result", error.message)));
  $("files-new-file").addEventListener("click", () => newEntry(false));
  $("files-new-folder").addEventListener("click", () => newEntry(true));
  $("files-content").addEventListener("input", () => controls());
  $("files-close").addEventListener("click", () => {
    if (filesDirty() && !window.confirm("Discard unsaved file changes?")) return;
    state.files.editing = null; controls();
  });
  $("files-save").addEventListener("click", () => {
    const path = state.files.editing, body = $("files-content").value;
    if (path === null) return;
    fileMutation(`Saving ${path}…`, async () => {
      await writeFile(path, body);
      state.files.baseline = body;
      return `Saved ${path}.`;
    });
  });
  $("files-upload").addEventListener("change", () => {
    const file = $("files-upload").files?.[0];
    if (!file) return;
    if (file.size > 64 * 1024 * 1024) { text("files-result", "Uploads are limited to 64 MiB."); return; }
    const path = joinPath(state.files.path, file.name);
    fileMutation(`Uploading ${file.name}…`, async () => {
      await writeFile(path, await file.arrayBuffer());
      $("files-upload").value = ""; state.busy = false;
      await loadFiles(state.files.path, state.files.root);
      return `Uploaded ${path}.`;
    });
  });

  setInterval(() => { if (!document.hidden && !state.busy && allowed("read")) refreshStatus(); }, 5000);
  setInterval(() => { if (!document.hidden && !state.busy && $("console-follow").checked) pollLogs(); }, 1000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden && !state.busy) refreshStatus(); });
  controls(); connect();
})();
