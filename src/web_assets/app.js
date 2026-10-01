(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    token: "", revision: null, baseline: "", writable: false,
    loaded: false, busy: false, polling: false, fetchingSource: false,
    session: 0, mutation: 0, remoteChanged: false, conflicted: false, online: false,
    serverButtons: [],
  };
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
    $("save").disabled = !available || !state.writable || !dirty() || state.remoteChanged;
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
    for (const { button, unavailable } of state.serverButtons) button.disabled = state.busy || unavailable;
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
    table("managed-servers", status.managed_servers || [], 6, (tr, server) => {
      cell(tr, server.name); cell(tr, server.state);
      cell(tr, `${count(server.players)} players · ${count(server.reservations)} attachments`);
      cell(tr, server.automatic_start ? "Enabled" : "Disabled");
      cell(tr, server.last_error || "—");
      const actions = cell(tr, ""); actions.className = "action-group";
      for (const action of ["start", "stop"]) {
        const button = document.createElement("button");
        button.type = "button"; button.className = "button subtle";
        button.textContent = action === "start" ? "Start" : "Stop";
        button.setAttribute("aria-label", `${button.textContent} ${server.name}`);
        const unavailable = action === "start"
          ? server.state === "starting" || server.state === "running" || server.state === "stopping"
          : number(server.players) > 0 || number(server.reservations) > 0 || server.state === "stopping" || (server.state === "stopped" && !server.automatic_start);
        state.serverButtons.push({ button, unavailable });
        button.addEventListener("click", () => serverOperation(server.name, action));
        actions.append(button);
      }
    });
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
      await refreshStatus();
      await loadSource();
    } catch (error) { handleError(error); }
    finally { state.busy = false; controls(); }
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
  $("source").addEventListener("input", controls);
  $("source").addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); validate(); }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") { event.preventDefault(); save(); }
  });
  window.addEventListener("beforeunload", (event) => {
    if (dirty()) { event.preventDefault(); event.returnValue = ""; }
  });
  $("auth-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (state.busy) return;
    state.token = $("token").value.trim(); $("token").value = ""; state.session += 1;
    text("auth-state", state.token ? "Token held in this tab" : "No token set");
    connect();
  });
  $("forget-token").addEventListener("click", () => {
    if (state.busy) return;
    state.token = ""; $("token").value = ""; state.session += 1;
    text("auth-state", "No token set"); connect();
  });
  $("refresh-status").addEventListener("click", () => refreshStatus());
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
  setInterval(() => { if (!document.hidden && !state.busy) refreshStatus(); }, 5000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden && !state.busy) refreshStatus(); });
  controls(); connect();
})();
