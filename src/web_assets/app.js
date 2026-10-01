(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    token: "", revision: null, baseline: "", writable: false,
    loaded: false, busy: false, polling: false, fetchingSource: false,
    session: 0, mutation: 0, remoteChanged: false, conflicted: false, online: false,
    serverButtons: [],
    permissions: null, groups: null, logServer: "", logCursor: null, logPolling: false, definitions: {}, definitionRevision: null, definitionDirty: false, definitionVersion: 0, definitionSection: "service_groups", definitionName: "", definitionInputs: new Map(),
  };
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
    for (const [id, permission] of [["config-panel", "config"], ["definitions-panel", "config"], ["deployments-panel", "deploy"], ["audit-panel", "audit"], ["extensions-panel", "extensions"]]) $(id).classList.toggle("hidden", !allowed(permission));
    $("live-console-panel").classList.toggle("hidden", !allowed("logs") && !allowed("console"));
  }
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
        const error = new Error(response.status === 404 ? fallback : data.error || fallback);
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
  function table(id, values, width, row) {
    const body = $(id);
    body.replaceChildren();
    for (const value of values) {
      const tr = document.createElement("tr");
      row(tr, value);
      body.append(tr);
    }
    if (!values.length) {
      const tr = document.createElement("tr"), td = document.createElement("td");
      td.colSpan = width; td.className = "empty"; td.textContent = "None configured";
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
      state.serverButtons.push({ button, unavailable: false });
      button.addEventListener("click", () => { if (!button.disabled) instanceOperation(group.name, "create", group.storage); });
      actions.append(button);
    });
    table("managed-servers", status.managed_servers || [], 9, (tr, server) => {
      cell(tr, server.name); cell(tr, server.state);
      cell(tr, `${count(server.players)} players · ${count(server.reservations)} attachments`);
      cell(tr, server.automatic_start ? "Enabled" : "Disabled");
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
        button.type = "button"; button.className = "button subtle";
        button.textContent = server.storage === "disposable" ? "Remove & delete files" : "Remove · keep files";
        button.setAttribute("aria-label", `${button.textContent}: ${server.name}`);
        button.title = server.storage === "disposable" ? "Stops the instance and permanently deletes its generated directory, including the world." : "Stops and unregisters the instance; its world and server files remain on disk.";
        const unavailable = number(server.players) > 0 || number(server.reservations) > 0 || ["stopping", "removing"].includes(server.state);
        state.serverButtons.push({ button, unavailable });
        button.addEventListener("click", () => { if (!button.disabled) instanceOperation(server.name, "remove", server.storage); });
        actions.append(button);
      }
    });
    const selected = $("console-server").value;
    $("console-server").replaceChildren();
    for (const server of [{ name: "", label: "Choose a server" }, ...(status.managed_servers || [])]) {
      const option = document.createElement("option"); option.value = server.name; option.textContent = server.label || server.name; $("console-server").append(option);
    }
    $("console-server").value = (status.managed_servers || []).some((s) => s.name === selected) ? selected : "";
    if (!$("console-server").value && state.logServer) { state.logServer = ""; state.logCursor = null; text("console-output", "Choose a server to view its logs."); }
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
      if (allowed("read")) await refreshStatus();
      if (allowed("config")) await loadSource();
      else { $("source").value = ""; state.baseline = ""; state.revision = null; state.loaded = false; }
      if (allowed("config")) await loadDefinitions();
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
    ["directory", "Instance directory ({name} required)", "text"], ["command", "Command arguments (one per line)", "array"],
    ["port_range.0", "First port", "number"], ["port_range.1", "Last port", "number"],
    ["template", "Template (optional)", "text"], ["storage", "Storage", ["persistent", "disposable"]],
    ["autostart", "Start new instances automatically", "boolean"], ["start_on_connect", "Start on player connection", "boolean"],
    ["idle_timeout_ms", "Idle timeout (ms; 0 disables)", "number"], ["start_timeout_ms", "Startup timeout (ms)", "number"],
    ["stop_timeout_ms", "Stop timeout (ms)", "number"], ["restart_delay_ms", "Recovery delay (ms)", "number"], ["restart_retries", "Recovery attempts", "number"],
    ["scaling_enabled", "Automatic scaling", "boolean"],
    ...[["min_instances", "Minimum instances"], ["max_instances", "Maximum instances"], ["spare_instances", "Spare instances"], ["capacity_per_instance", "Players per instance"], ["target_occupancy_percent", "Target occupancy (%)"], ["queue_threshold", "Queue threshold"], ["cooldown_ms", "Scaling cooldown (ms)"]].map(([key, label]) => [`scaling.${key}`, label, "number"]),
  ];
  const templateFields = [["server_jar", "Server jar path", "text"], ["plugins", "Plugin paths (one per line)", "array"], ["configs", "Configuration directory (optional)", "text"], ["map", "Map directory (optional)", "text"]];
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
        for (const item of type === "boolean" ? ["false", "true"] : type) { const option = document.createElement("option"); option.value = item; option.textContent = item === "true" ? "Enabled" : item === "false" ? "Disabled" : item; input.append(option); }
        input.value = String(current ?? (type === "boolean" ? false : type[0]));
      } else { if (type !== "array") input.type = type === "number" ? "number" : "text"; if (type === "number") { input.min = "0"; input.step = "1"; } input.value = type === "array" ? (current || []).join("\n") : current ?? ""; }
      input.addEventListener("input", () => { state.definitionDirty = true; state.definitionVersion += 1; });
      state.definitionInputs.set(key, { input, type }); wrapper.append(title, input); $("definition-fields").append(wrapper);
    }
    state.definitionDirty = false; controls();
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
  }
  function definitionValue() {
    const value = {}, scaling = {}, ports = [], scalingEnabled = state.definitionInputs.get("scaling_enabled")?.input.value === "true";
    for (const [key, { input, type }] of state.definitionInputs) {
      if (key === "scaling_enabled") continue;
      const raw = String(input.value);
      if (key.startsWith("scaling.") && !scalingEnabled) continue;
      if (raw === "") continue;
      let parsed = type === "array" ? raw.split("\n").filter((line) => line.trim()) : type === "number" ? Number(raw) : type === "boolean" ? raw === "true" : raw;
      if (type === "number" && (!Number.isSafeInteger(parsed) || parsed < 0)) throw new Error("Numeric fields must be nonnegative integers.");
      if (key.startsWith("scaling.")) scaling[key.split(".")[1]] = parsed;
      else if (key.startsWith("port_range.")) ports[Number(key.split(".")[1])] = parsed;
      else value[key] = parsed;
    }
    if (scalingEnabled) value.scaling = scaling;
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
  async function loadDeployments() {
    const session = state.session, data = await request("/api/deployments");
    if (session !== state.session) return;
    state.deploymentRevision = data.revision;
    table("deployments", data.deployments || [], 5, (tr, deployment) => {
      cell(tr, time(deployment.timestamp_unix_ms)); cell(tr, deployment.actor); cell(tr, deployment.action); cell(tr, deployment.revision, "address");
      const actions = cell(tr, ""), button = document.createElement("button"); button.type = "button"; button.className = "button secondary"; button.textContent = "Roll back";
      button.addEventListener("click", async () => {
        if (state.busy || !allowed("deploy")) return;
        if (dirty() || state.definitionDirty) { text("deployment-result", "Save or discard editor changes before rolling back."); return; }
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
    });
  }
  async function loadAudit() {
    const session = state.session, data = await request("/api/audit");
    if (session !== state.session) return;
    table("audit-records", data.records || [], 5, (tr, record) => { cell(tr, time(record.timestamp_unix_ms)); cell(tr, record.actor); cell(tr, record.action); cell(tr, record.target || "—"); cell(tr, `${record.outcome}${record.status ? ` (${record.status})` : ""}`); });
    text("audit-result", data.durable ? "Records are retained on disk." : "Records are held in memory for this process.");
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
    try {
      const outcome = await request(action === "create"
        ? `/api/groups/${encodeURIComponent(name)}/instances`
        : `/api/instances/${encodeURIComponent(name)}`, { method: action === "create" ? "POST" : "DELETE", body: "{}" });
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
      text("server-result", error.message || "The instance request failed.");
      $("server-result").className = "operation-result error";
    } finally { state.busy = false; controls(); }
  }
  $("source").addEventListener("input", controls);
  $("source").addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); validate(); }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") { event.preventDefault(); save(); }
  });
  window.addEventListener("beforeunload", (event) => {
    if (dirty() || state.definitionDirty) { event.preventDefault(); event.returnValue = ""; }
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
    if ((dirty() || state.definitionDirty) && !window.confirm("Switch operator and discard unsaved editor changes?")) return false;
    state.loaded = false; state.baseline = ""; state.revision = null; state.remoteChanged = false; state.conflicted = false;
    state.permissions = []; state.logServer = ""; state.logCursor = null; state.definitions = {}; state.definitionRevision = null; state.definitionDirty = false; state.definitionVersion += 1;
    $("source").value = ""; $("console-server").replaceChildren(); $("definition-fields").replaceChildren();
    for (const id of ["console-output", "config-path", "revision", "deployment-result", "definition-result", "console-result", "extension-output"]) text(id, "");
    for (const id of ["deployments", "audit-records", "managed-servers", "service-groups", "backends", "routes"]) $(id).replaceChildren();
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
      text("console-result", data.message || "Command sent; check logs for its result."); await pollLogs();
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
  $("deployments-refresh").addEventListener("click", async () => { try { await loadDeployments(); } catch (error) { text("deployment-result", error.message); } });
  $("audit-refresh").addEventListener("click", async () => { try { await loadAudit(); } catch (error) { text("audit-result", error.message); } });
  $("audit-panel").addEventListener("toggle", () => { if ($("audit-panel").open && allowed("audit")) loadAudit().catch((error) => text("audit-result", error.message)); });
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
  setInterval(() => { if (!document.hidden && !state.busy && allowed("read")) refreshStatus(); }, 5000);
  setInterval(() => { if (!document.hidden && !state.busy && $("console-follow").checked) pollLogs(); }, 1000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden && !state.busy) refreshStatus(); });
  controls(); connect();
})();
