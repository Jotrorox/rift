(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    token: "", revision: null, baseline: "", writable: false,
    loaded: false, busy: false, polling: false, fetchingSource: false,
    session: 0, mutation: 0, remoteChanged: false, conflicted: false, online: false,
    serverButtons: [], groupButtons: [], managedServers: [], view: "overview",
    permissions: null, groups: null, logServer: "", logCursor: null, logPolling: false, definitions: {}, definitionRevision: null, definitionDirty: false, definitionVersion: 0, definitionSection: "service_groups", definitionName: "", definitionInputs: new Map(),
    // Unsaved edits on the server list, tab list and permission group forms.
    settingsDirty: { serverList: false, tabList: false },
    // The permission views: loaded groups, tracks, the selected group and player.
    perms: permsState(),
    // Template downloads and the file browser.
    templateButtons: [], files: { root: "templates", path: "", editing: null, baseline: "", buttons: [] },
    // Plugin restarts and the plugin log tail.
    pluginButtons: [], pluginLog: "", pluginLogPolling: false,
  };
  function permsState() {
    return { groups: [], tracks: [], group: "", groupDetail: null, user: "", userDetail: null, users: [], registered: [], events: [], expanded: new Set(),
      buttons: [], editing: { "perm-group": null, "perm-user": null }, trackGroups: [], trackDirty: false };
  }
  const settingsDirty = () => Object.values(state.settingsDirty).some(Boolean);
  const allowed = (permission) => state.permissions === null || state.permissions.includes(permission);
  // Plugins act on the whole proxy, so group-scoped operators see none.
  const pluginsVisible = () => allowed("read") && !state.groups;
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
    for (const { button, unavailable } of [...state.serverButtons, ...state.groupButtons]) button.disabled = state.busy || unavailable || !allowed("servers");
    $("console-send").disabled = state.busy || !state.logServer || !allowed("console");
    $("definition-save").disabled = state.busy || !state.definitionRevision || !allowed("config");
    $("definition-delete").disabled = state.busy || !state.definitionRevision || !$("definition-select").value || !allowed("config");
    for (const id of ["server-list-save", "tab-list-save"]) $(id).disabled = state.busy || !state.definitionRevision || !allowed("config");
    for (const button of state.perms.buttons) button.disabled = state.busy || !permsWrite() || Boolean(button.edge);
    for (const id of ["perm-group-create", "perm-group-save", "perm-group-delete", "perm-group-node-save", "perm-user-node-save", "perm-track-save", "perm-track-add",
      "perm-user-primary-save", "perm-user-parent-add", "perm-user-promote", "perm-user-demote"]) $(id).disabled = state.busy || !permsWrite();
    $("perm-group-delete").disabled = state.busy || !permsWrite() || state.perms.group === "default";
    $("perm-track-delete").disabled = state.busy || !permsWrite() || !$("perm-track-select").value;
    $("perm-track-add").disabled = state.busy || !permsWrite() || !$("perm-track-add-group").value;
    // Read-only operators see the data without the editing forms.
    for (const id of ["perm-group-create-form", "perm-group-delete", "perm-group-node-form", "perm-user-node-form", "perm-track-form", "perm-user-primary-form", "perm-user-parent-form", "perm-user-track-form"]) $(id).classList.toggle("hidden", !permsWrite());
    for (const id of ["perm-group-weight", "perm-group-display"]) $(id).disabled = !permsWrite();
    $("perm-group-save").classList.toggle("hidden", !permsWrite());
    // Fields that only matter while their setting is on.
    for (const id of ["server-list-motd", "server-list-maintenance-motd", "server-list-version", "server-list-max-players", "server-list-sample"]) $(id).disabled = !$("server-list-enabled").checked;
    for (const id of ["tab-list-header", "tab-list-footer", "tab-list-players"]) $(id).disabled = !$("tab-list-enabled").checked;
    for (const [id, permission] of [["config-panel", "config"], ["definitions-panel", "config"], ["server-list-panel", "config"], ["server-list-lua-panel", "config"], ["tab-list-panel", "config"], ["deployments-panel", "deploy"], ["audit-panel", "audit"], ["extensions-panel", "extensions"]]) $(id).classList.toggle("hidden", !allowed(permission));
    $("live-console-panel").classList.toggle("hidden", !allowed("logs") && !allowed("console"));
    for (const { button, unavailable } of state.templateButtons) button.disabled = state.busy || unavailable || !allowed("servers");
    const filesDirty = state.files.editing !== null && $("files-content").value !== state.files.baseline;
    for (const id of ["files-open", "files-up", "files-new-file", "files-new-folder", "files-upload", "files-refresh"]) $(id).disabled = state.busy || !allowed("files");
    $("files-save").disabled = state.busy || !allowed("files") || state.files.editing === null || !filesDirty;
    for (const button of state.files.buttons) button.disabled = state.busy || !allowed("files");
    for (const button of state.pluginButtons) button.disabled = state.busy || !allowed("plugins");
    $("plugin-log-panel").classList.toggle("hidden", !allowed("logs"));
    $("files-editor").classList.toggle("hidden", state.files.editing === null);
    text("files-edit-state", filesDirty ? "Unsaved changes" : "Saved");
    $("files-edit-state").className = `tag ${filesDirty ? "warning" : "good"}`;
    const visible = { script: allowed("config"), definitions: allowed("config"), extensions: allowed("extensions"),
      "server-list": allowed("config"), "tab-list": allowed("config"),
      "perm-groups": permsRead(), "perm-users": permsRead(), "perm-nodes": permsRead(), "perm-activity": permsRead(),
      console: allowed("logs") || allowed("console"), deployments: allowed("deploy"), audit: allowed("audit"), files: allowed("files"), plugins: pluginsVisible() };
    for (const [id] of views) $(`nav-${id}`).classList.toggle("hidden", visible[id] === false);
    for (const [section, ids] of sections) $(`section-${section}`).classList.toggle("hidden", ids.every((id) => visible[id] === false));
    showView();
  }
  const views = [["overview", "Overview"], ["routing", "Routing"], ["server-list", "Server list"], ["tab-list", "Tab list"], ["metrics", "Metrics"], ["servers", "Servers"], ["console", "Console"], ["groups", "Service groups"], ["files", "Files"],
    ["script", "Lua script"], ["definitions", "Groups & templates"], ["plugins", "Plugins"], ["extensions", "Extensions"], ["reference", "Reference"],
    ["perm-groups", "Permission groups"], ["perm-users", "Players"], ["perm-nodes", "Permission nodes"], ["perm-activity", "Permission activity"], ["deployments", "Deployments"], ["audit", "Audit log"]];
  const sections = [["configuration", ["script", "definitions", "plugins", "extensions"]], ["permissions", ["perm-groups", "perm-users", "perm-nodes", "perm-activity"]], ["activity", ["deployments", "audit"]]];
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
    if (changed && current.startsWith("perm-") && permsRead()) {
      const failed = (id) => (error) => permsOutcome(id, error.message, "error");
      if (current === "perm-groups") loadPermGroups().catch(failed("perm-groups-result"));
      if (current === "perm-users") loadPermGroups().then(() => state.perms.users.length ? null : searchPermUsers()).catch(failed("perm-users-result"));
      if (current === "perm-nodes") loadRegistered().catch(failed("perm-holders-result"));
      if (current === "perm-activity") loadActivity().catch(failed("perm-activity-result"));
    }
    if (changed && current === "groups" && allowed("read")) loadTemplates().catch((error) => text("template-result", error.message));
    if (changed && current === "files" && allowed("files")) loadFiles().catch((error) => text("files-result", error.message));
    if (changed && current === "plugins" && pluginsVisible()) loadPlugins().catch((error) => text("plugin-result", error.message));
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
          ? "Access denied. Enter web.token, or the token Rift printed at startup, under API access."
          : response.status === 404
            ? "The admin API is unavailable. Enable web.api in the Lua configuration, or check that this is the admin service address."
            : `Request failed (HTTP ${response.status}).`;
        const error = new Error(response.status === 404 && !path.startsWith("/api/operations/") && !data.code ? fallback : data.error || fallback);
        error.status = response.status; error.code = data.code;
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
  function renderServers() {
    const group = $("servers-group").value;
    const servers = state.managedServers.filter((server) => !group || server.group === group);
    state.serverButtons = [];
    table("managed-servers", servers, 10, (tr, server) => {
      cell(tr, server.name); cell(tr, server.state, `state ${server.state}`);
      cell(tr, `${count(server.players)} players · ${count(server.reservations)} attachments`);
      cell(tr, wakeLabel(server));
      cell(tr, server.storage === "disposable" ? "Disposable game" : "Persistent world");
      cell(tr, server.template || "Prepared directory");
      cell(tr, server.group ? `${server.group} · ${server.address}` : server.address, "address");
      cell(tr, server.node || "local");
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
    }, group ? `No instances in ${group} yet.` : "No managed servers yet. Create one from a service group.");
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
    state.groupButtons = [];
    table("service-groups", status.service_groups || [], 7, (tr, group) => {
      const names = group.instances || [], summary = group.summary || {}, enabled = (key) => number(summary[key]);
      cell(tr, group.name);
      const badges = cell(tr, "");
      for (const [label, kind] of groupStatus(group)) {
        const tag = document.createElement("span"); tag.className = `tag ${kind}`; tag.textContent = label; badges.append(tag);
      }
      cell(tr, (group.port_range || []).join("–"));
      cell(tr, group.storage === "disposable" ? "Disposable game" : "Persistent world");
      cell(tr, group.template || "Prepared directories");
      cell(tr, instanceSummary(group)).title = names.join(", ");
      const actions = cell(tr, ""); actions.className = "action-group";
      const button = (label, aria, unavailable, title, click, className = "button subtle") => {
        const element = document.createElement("button");
        element.type = "button"; element.className = className; element.textContent = label;
        element.setAttribute("aria-label", aria);
        if (title) element.title = title;
        state.groupButtons.push({ button: element, unavailable });
        element.addEventListener("click", () => { if (!element.disabled) click(); });
        actions.append(element);
        return element;
      };
      const full = Boolean(group.scaling) && names.length >= number(group.scaling.max_instances);
      button("Create instance", `Create instance in ${group.name}`, full || Boolean(group.suspended),
        group.suspended ? "This group is stopped. Start it before creating instances."
          : full ? "This group has reached its scaling maximum. Remove an instance or raise max_instances first." : "",
        () => instanceOperation(group.name, "create", group.storage), "button secondary");
      // Without a summary (older servers) every action stays available.
      const allUp = Boolean(group.summary) && names.length > 0 && enabled("running") + enabled("starting") === names.length;
      button("Start all", `Start every instance of ${group.name}`, !group.suspended && allUp,
        group.suspended ? "Resume scaling and start every instance." : "Start every instance that is not running.",
        () => groupOperation(group, "start"));
      button("Stop all", `Stop every instance of ${group.name}`, Boolean(group.suspended),
        group.suspended ? "This group is already stopped." : "Keep new players out, pause scaling and stop every instance once it is empty.",
        () => {
          const players = enabled("players");
          if (!window.confirm(`Stop every instance of ${group.name}? New players can't join and scaling pauses until you start the group.${players ? ` Instances with players (${count(players)} now) keep running until they leave.` : ""}`)) return;
          groupOperation(group, "stop");
        });
      const dev = group.mode === "dev";
      button(dev ? "Leave dev mode" : "Dev mode", `${dev ? "Leave dev mode for" : "Put into dev mode:"} ${group.name}`, false,
        dev ? "Let everyone join again and resume scaling." : `Only players with ${group.dev_permission || "the group's dev permission"} can join; scaling pauses.`,
        () => {
          if (!dev && !window.confirm(`Put ${group.name} into dev mode? Only players with the permission ${group.dev_permission || `rift.group.${group.name}.dev`} can join, and automatic scaling pauses. Players already connected stay.`)) return;
          groupOperation(group, "mode", dev ? "active" : "dev");
        });
      if (allowed("config")) {
        const edit = document.createElement("button");
        edit.type = "button"; edit.className = "button subtle"; edit.textContent = "Edit";
        edit.setAttribute("aria-label", `Edit the ${group.name} definition`);
        edit.addEventListener("click", () => editGroup(group.name));
        actions.append(edit);
      }
    });
    const groupFilter = $("servers-group"), filtered = groupFilter.value;
    groupFilter.replaceChildren();
    for (const [value, label] of [["", "All groups"], ...(status.service_groups || []).map((group) => [group.name, group.name])]) {
      const option = document.createElement("option"); option.value = value; option.textContent = label; groupFilter.append(option);
    }
    groupFilter.value = (status.service_groups || []).some((group) => group.name === filtered) ? filtered : "";
    const nodes = status.nodes || [];
    $("nodes-panel").hidden = !nodes.length;
    text("node-count", `${nodes.filter((node) => node.state === "online").length} of ${nodes.length} online`);
    table("nodes", nodes, 6, (tr, node) => {
      cell(tr, node.name);
      cell(tr, node.state === "never_connected" ? "never connected" : node.state, `state ${node.state === "online" ? "running" : node.state === "unreachable" ? "failed" : "stopped"}`);
      cell(tr, node.address, "address");
      cell(tr, (node.labels || []).join(", ") || "—");
      cell(tr, `${count(node.instances)} of ${count(node.max_instances)}`);
      cell(tr, node.memory_mb ? `${count(node.allocated_memory_mb)} of ${count(node.memory_mb)} MiB` : `${count(node.allocated_memory_mb)} MiB, no limit`);
    }, "No nodes configured.");
    state.managedServers = status.managed_servers || [];
    renderServers();
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
      if (allowed("config")) await loadDefinitions();
      if (permsRead()) await loadPermGroups().catch((error) => permsOutcome("perm-groups-result", error.message, "error"));
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
    ["software.url", "Download URL (type url)", "text"], ["software.sha256", "SHA-256 (required for type url; pins an exact version otherwise)", "text"],
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
  // Permissions: groups, users, registered nodes and the change log, all
  // through /api/perms. `perms` reads; `config` writes.
  const permsRead = () => allowed("perms") || allowed("config");
  const permsWrite = () => allowed("config");
  const kindLabels = { permission: "Permission", inherit: "Parent", prefix: "Prefix", suffix: "Suffix", meta: "Meta", weight: "Weight", display_name: "Display name" };
  const subjectLabel = (subject) => subject?.user ? `user:${subject.user}` : subject?.group ? `group:${subject.group}` : "—";
  const contextText = (contexts) => Object.entries(contexts || {}).flatMap(([key, value]) => [value].flat().map((item) => `${key}=${item}`)).join(", ");
  const expiryText = (at) => at ? new Date(number(at) * 1000).toLocaleString() : "Never";
  // `key=value` pairs separated by commas, semicolons or lines; a repeated key holds several values.
  function parseContexts(raw) {
    const contexts = {};
    for (const part of String(raw || "").split(/[,;\n]/).map((item) => item.trim()).filter(Boolean)) {
      const match = /^([A-Za-z0-9_.:-]{1,64})\s*=\s*([^=;\u0000-\u001f]{1,256})$/.exec(part);
      if (!match) throw new Error(`“${part}” isn’t a key=value context.`);
      const key = match[1].toLowerCase(), value = match[2].trim().toLowerCase();
      contexts[key] = Object.hasOwn(contexts, key) ? [contexts[key], value].flat() : value;
    }
    return contexts;
  }
  // Unix seconds from an expiry select ("" never, seconds from now, or "date").
  function expiryValue(selectId, dateId) {
    const choice = $(selectId).value;
    if (!choice) return undefined;
    if (choice !== "date") return Math.floor(Date.now() / 1000) + number(choice);
    const at = Math.floor(Date.parse($(dateId).value) / 1000);
    if (!Number.isFinite(at)) throw new Error("Choose an expiry date and time.");
    if (at <= Date.now() / 1000) throw new Error("The expiry must be in the future.");
    return at;
  }
  const localInput = (seconds) => {
    const date = new Date(number(seconds) * 1000);
    return new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
  };
  // The key column of a typed node.
  function nodeKey(node) {
    switch (node.kind) {
      case "permission": case "meta": return node.key;
      case "inherit": return node.group;
      case "prefix": case "suffix": return `priority ${node.priority}`;
      default: return kindLabels[node.kind] || node.kind;
    }
  }
  function nodeValue(node) {
    switch (node.kind) {
      case "permission": return node.value === false ? "Deny" : "Grant";
      case "inherit": return node.value === false ? "Block" : "Inherit";
      case "prefix": case "suffix": return node.text;
      case "meta": return node.value;
      case "weight": return String(node.weight);
      case "display_name": return node.name;
      default: return "—";
    }
  }
  // Ordered rows of the inheritance tree: each group under its heaviest
  // existing parent, siblings by weight then name, default first.
  function groupTree(groups) {
    const byName = new Map(groups.map((group) => [group.name, group]));
    const order = (list) => list.sort((a, b) => (a.name === "default" ? -1 : b.name === "default" ? 1 : 0) || number(b.weight) - number(a.weight) || a.name.localeCompare(b.name));
    const children = new Map(), roots = [];
    for (const group of groups) {
      const parents = (group.parents || []).filter((node) => node.value !== false && node.group !== group.name && byName.has(node.group)).map((node) => byName.get(node.group));
      const parent = order(parents)[0];
      if (parent) { if (!children.has(parent.name)) children.set(parent.name, []); children.get(parent.name).push(group); } else roots.push(group);
    }
    const rows = [], seen = new Set();
    const visit = (group, depth) => {
      if (seen.has(group.name)) return;
      seen.add(group.name); rows.push({ group, depth });
      for (const child of order(children.get(group.name) || [])) visit(child, depth + 1);
    };
    for (const group of order(roots)) visit(group, 0);
    // Groups that only inherit each other in a cycle.
    for (const group of order(groups.filter((group) => !seen.has(group.name)))) visit(group, 0);
    return rows;
  }
  // Before/after lines of one change-log effect.
  function opDiff(op) {
    const node = op.after || op.before || {};
    const title = op.op === "node" ? `${subjectLabel(op.subject)} · ${kindLabels[node.kind] || node.kind} ${nodeKey(node)}`
      : op.op === "group" ? `group:${op.name} · weight and display name`
        : op.op === "track" ? `track:${op.name}` : `user:${op.user} · primary group`;
    const flat = (value) => value === null || value === undefined ? {} : typeof value === "object" && !Array.isArray(value)
      ? Object.fromEntries(Object.entries(value).map(([key, item]) => [key, typeof item === "object" ? key === "contexts" ? contextText(item) : JSON.stringify(item) : String(item)]))
      : { value: Array.isArray(value) ? value.join(" → ") : String(value) };
    const before = flat(op.before), after = flat(op.after), lines = [];
    for (const key of [...new Set([...Object.keys(before), ...Object.keys(after)])]) {
      if (before[key] === after[key]) lines.push({ sign: " ", text: `${key}: ${after[key]}` });
      else {
        if (Object.hasOwn(before, key)) lines.push({ sign: "-", text: `${key}: ${before[key]}` });
        if (Object.hasOwn(after, key)) lines.push({ sign: "+", text: `${key}: ${after[key]}` });
      }
    }
    return { title: `${op.before === null || op.before === undefined ? "Added" : op.after === null || op.after === undefined ? "Removed" : "Changed"} ${title}`, lines };
  }
  function permsButton(label, onClick, { className = "button subtle", aria = "", write = true } = {}) {
    const button = document.createElement("button");
    button.type = "button"; button.className = className; button.textContent = label;
    if (aria) button.setAttribute("aria-label", aria);
    button.addEventListener("click", () => { if (!button.disabled) onClick(); });
    if (write) state.perms.buttons.push(button);
    return button;
  }
  function permsOutcome(id, message, kind = "") { text(id, message); $(id).className = `operation-result ${kind}`; }
  // Run one write: busy state, result line, and a reload afterwards.
  async function permsMutation(resultId, message, work) {
    if (state.busy || !permsWrite()) return false;
    state.busy = true; controls(); permsOutcome(resultId, message);
    try {
      const done = await work();
      permsOutcome(resultId, done, "success");
      return true;
    } catch (error) {
      if ((error.status === 401 || error.status === 403) && !error.code) $("auth-panel").open = true;
      permsOutcome(resultId, error.message, "error");
      return false;
    } finally { state.busy = false; controls(); }
  }
  const appliedText = (applied, changed, unchanged) => number(applied?.ops) > 0 ? `${changed} Revision ${applied.revision}.` : unchanged;
  const json = (value) => JSON.stringify(value);
  function fillSelect(id, values, { blank = null, keep = true } = {}) {
    const select = $(id), previous = select.value;
    select.replaceChildren();
    for (const [value, label] of [...(blank === null ? [] : [["", blank]]), ...values.map((value) => Array.isArray(value) ? value : [value, value])]) {
      const option = document.createElement("option"); option.value = value; option.textContent = label; select.append(option);
    }
    const options = [...(blank === null ? [] : [""]), ...values.map((value) => Array.isArray(value) ? value[0] : value)];
    select.value = keep && options.includes(previous) ? previous : options[0] ?? "";
  }

  // Groups and tracks.
  async function loadPermGroups() {
    const session = state.session;
    const [groups, tracks] = await Promise.all([request("/api/perms/groups"), request("/api/perms/tracks")]);
    if (session !== state.session) return;
    state.perms.groups = Array.isArray(groups.groups) ? groups.groups : [];
    state.perms.tracks = Array.isArray(tracks.tracks) ? tracks.tracks : [];
    renderPermGroups();
    if (state.perms.group && !state.perms.groups.some((group) => group.name === state.perms.group)) state.perms.group = "";
    if (state.perms.group) await loadPermGroup(state.perms.group);
    else renderPermGroup(null);
  }
  function renderPermGroups() {
    const groups = state.perms.groups, names = groups.map((group) => group.name);
    text("perm-group-count", `${groups.length} ${groups.length === 1 ? "group" : "groups"}`);
    table("perm-groups", groupTree(groups), 6, (tr, { group, depth }) => {
      tr.setAttribute("aria-current", String(group.name === state.perms.group));
      const name = cell(tr, "", `tree-depth-${Math.min(depth, 5)}`), open = document.createElement("button");
      open.type = "button"; open.className = "text-link"; open.textContent = group.name;
      open.setAttribute("aria-label", `Edit group ${group.name}${depth ? `, level ${depth + 1}` : ""}`);
      open.addEventListener("click", () => selectPermGroup(group.name));
      name.append(open);
      if (group.display_name) { const span = document.createElement("span"); span.className = "muted"; span.textContent = ` ${group.display_name}`; name.append(span); }
      cell(tr, String(number(group.weight)), "metric-value");
      cell(tr, (group.parents || []).map((node) => `${node.value === false ? "not " : ""}${node.group}${node.contexts ? ` (${contextText(node.contexts)})` : ""}`).join(", ") || (group.name === "default" ? "Everyone" : "—"));
      cell(tr, count(group.nodes), "metric-value");
      cell(tr, (group.tracks || []).join(", ") || "—");
      const actions = cell(tr, ""); actions.className = "action-group";
      actions.append(permsButton("Edit", () => selectPermGroup(group.name), { aria: `Edit ${group.name}`, write: false }));
    }, "No groups yet. Create one below.");
    const nonDefault = names.filter((name) => name !== "default");
    fillSelect("perm-user-group", names, { blank: "Any group" });
    fillSelect("perm-user-parent-group", nonDefault);
    fillSelect("perm-user-primary", names, { blank: "Automatic (heaviest parent)" });
    fillSelect("perm-user-track", state.perms.tracks.map((track) => track.name));
    renderPermTracks();
    controls();
  }
  async function selectPermGroup(name) {
    state.perms.group = name; state.perms.editing["perm-group"] = null;
    window.location.hash = "#perm-groups";
    try { await loadPermGroup(name); } catch (error) { permsOutcome("perm-group-result", error.message, "error"); }
    $("perm-group-weight").focus?.();
  }
  async function loadPermGroup(name) {
    const session = state.session, group = await request(`/api/perms/groups/${encodeURIComponent(name)}`);
    if (session !== state.session || state.perms.group !== name) return;
    renderPermGroup(group);
  }
  function renderPermGroup(group) {
    state.perms.groupDetail = group;
    $("perm-group-panel").classList.toggle("hidden", !group);
    $("perm-group-nodes-panel").classList.toggle("hidden", !group);
    for (const row of $("perm-groups").children) row.setAttribute?.("aria-current", String(Boolean(group) && row.children?.[0]?.children?.[0]?.textContent === group.name));
    if (!group) return controls();
    text("perm-group-heading", `Group ${group.name}`);
    $("perm-group-weight").value = String(number(group.weight));
    $("perm-group-display").value = group.display_name || "";
    renderNodes("perm-group", group.nodes || []);
  }
  function renderPermTracks() {
    const tracks = state.perms.tracks;
    text("perm-track-count", `${tracks.length} ${tracks.length === 1 ? "track" : "tracks"}`);
    table("perm-tracks", tracks, 3, (tr, track) => {
      cell(tr, track.name, "address"); cell(tr, (track.groups || []).join(" → "));
      const actions = cell(tr, ""); actions.className = "action-group";
      actions.append(permsButton("Edit", () => selectPermTrack(track.name), { aria: `Edit track ${track.name}`, write: false }));
    }, "No tracks yet. Name one below and add groups in order.");
    fillSelect("perm-track-select", tracks.map((track) => track.name), { blank: "New track" });
    if (!state.perms.trackDirty) selectPermTrack($("perm-track-select").value, { focus: false });
  }
  function selectPermTrack(name, { focus = true } = {}) {
    if (state.perms.trackDirty && focus && !window.confirm("Discard unsaved track changes?")) return;
    const track = state.perms.tracks.find((item) => item.name === name);
    $("perm-track-select").value = track ? name : "";
    $("perm-track-name").value = track ? name : "";
    state.perms.trackGroups = track ? [...track.groups] : [];
    state.perms.trackDirty = false;
    renderTrackGroups();
    if (focus) $("perm-track-name").focus?.();
  }
  function renderTrackGroups() {
    const list = $("perm-track-groups"), groups = state.perms.trackGroups;
    state.perms.buttons = state.perms.buttons.filter((button) => !button.trackItem);
    list.replaceChildren();
    groups.forEach((name, index) => {
      const item = document.createElement("li"), label = document.createElement("span"), actions = document.createElement("span");
      label.textContent = name; label.className = "mono"; actions.className = "action-group";
      const move = (offset) => { const target = index + offset; [groups[index], groups[target]] = [groups[target], groups[index]]; state.perms.trackDirty = true; renderTrackGroups(); };
      const up = permsButton("Up", () => move(-1), { aria: `Move ${name} up` }), down = permsButton("Down", () => move(1), { aria: `Move ${name} down` });
      const remove = permsButton("Remove", () => { groups.splice(index, 1); state.perms.trackDirty = true; renderTrackGroups(); }, { className: "button destructive", aria: `Remove ${name} from the track` });
      for (const button of [up, down, remove]) button.trackItem = true;
      up.edge = index === 0; down.edge = index === groups.length - 1;
      actions.append(up, down, remove); item.append(label, actions); list.append(item);
    });
    if (!groups.length) { const item = document.createElement("li"); item.className = "muted"; item.textContent = "No groups on this track yet."; list.append(item); }
    fillSelect("perm-track-add-group", state.perms.groups.map((group) => group.name).filter((name) => !groups.includes(name)));
    controls();
  }

  // The node editor shared by groups and users; `p` is the element prefix.
  function nodeBase(p) {
    return p === "perm-group" ? `/api/perms/groups/${encodeURIComponent(state.perms.group)}` : `/api/perms/users/${encodeURIComponent(state.perms.user)}`;
  }
  function renderNodes(p, entries) {
    state.perms.buttons = state.perms.buttons.filter((button) => button.nodeEditor !== p);
    text(`${p}-node-count`, `${entries.length} ${entries.length === 1 ? "node" : "nodes"}`);
    table(`${p}-nodes`, entries, 7, (tr, { id, node }) => {
      cell(tr, kindLabels[node.kind] || node.kind); cell(tr, nodeKey(node), "address"); cell(tr, nodeValue(node));
      cell(tr, contextText(node.contexts) || "Everywhere"); cell(tr, expiryText(node.expires_at));
      const source = cell(tr, ""), tag = document.createElement("span");
      tag.className = `tag ${node.source && node.source !== "store" ? "warning" : ""}`; tag.textContent = node.source || "store"; source.append(tag);
      const actions = cell(tr, ""); actions.className = "action-group";
      // Weight and display name are edited with the group itself.
      if (node.kind === "weight" || node.kind === "display_name") return;
      const edit = permsButton("Edit", () => editNode(p, id, node), { aria: `Edit ${nodeKey(node)}` });
      const remove = permsButton("Remove", () => removeNode(p, id, node), { className: "button destructive", aria: `Remove ${nodeKey(node)}` });
      edit.nodeEditor = p; remove.nodeEditor = p;
      actions.append(edit, remove);
    }, p === "perm-group" ? "This group has no nodes yet." : "This player has no nodes of their own. They still inherit default.");
    if (!state.perms.editing[p]) resetNodeForm(p);
  }
  function nodeFormKind(p) {
    const kind = $(`${p}-node-kind`).value;
    const labels = { permission: ["Permission node", "essentials.fly"], inherit: ["Group", "vip"], prefix: ["Priority", "100"], suffix: ["Priority", "100"], meta: ["Meta key", "homes"] }[kind] || ["Key", ""];
    text(`${p}-node-key-label`, labels[0]); $(`${p}-node-key`).placeholder = labels[1];
    $(`${p}-node-grant-field`).classList.toggle("hidden", kind !== "permission" && kind !== "inherit");
    $(`${p}-node-value-field`).classList.toggle("hidden", kind === "permission" || kind === "inherit");
    text(`${p}-node-value-label`, kind === "meta" ? "Value" : "Text");
    const grant = $(`${p}-node-grant`), chosen = grant.value;
    grant.replaceChildren();
    for (const [value, label] of kind === "inherit" ? [["true", "Inherit"], ["false", "Block"]] : [["true", "Grant"], ["false", "Deny"]]) {
      const option = document.createElement("option"); option.value = value; option.textContent = label; grant.append(option);
    }
    grant.value = chosen === "false" ? "false" : "true";
  }
  function nodeFromForm(p) {
    const kind = $(`${p}-node-kind`).value, key = $(`${p}-node-key`).value.trim(), value = $(`${p}-node-value`).value, grant = $(`${p}-node-grant`).value !== "false";
    let node;
    if (kind === "permission") {
      if (key.length > 256 || !/^(\*|[a-z0-9_:-]+(\.[a-z0-9_:-]+)*(\.\*)?)$/i.test(key)) throw new Error(`“${key}” isn’t a permission node, prefix.* wildcard or *.`);
      node = { kind, key: key.toLowerCase(), value: grant };
    } else if (kind === "inherit") {
      if (!/^[a-z0-9_.-]{1,64}$/i.test(key)) throw new Error("Enter a group name using letters, digits, dots, underscores or hyphens.");
      node = { kind, group: key.toLowerCase(), value: grant };
    } else if (kind === "prefix" || kind === "suffix") {
      if (!/^-?\d{1,10}$/.test(key) || Math.abs(Number(key)) > 2147483647) throw new Error("The priority must be a whole number.");
      if (!value) throw new Error(`Enter the ${kind} text.`);
      node = { kind, priority: Number(key), text: value };
    } else if (kind === "meta") {
      if (!/^[a-z0-9_.:-]{1,64}$/i.test(key)) throw new Error("Enter a meta key using letters, digits, dots, colons, underscores or hyphens.");
      node = { kind, key: key.toLowerCase(), value };
    } else throw new Error("Choose a node kind.");
    const contexts = parseContexts($(`${p}-node-contexts`).value);
    if (Object.keys(contexts).length) node.contexts = contexts;
    const expires = expiryValue(`${p}-node-expiry`, `${p}-node-date`);
    if (expires) node.expires_at = expires;
    return node;
  }
  function resetNodeForm(p) {
    state.perms.editing[p] = null;
    for (const field of ["key", "value", "contexts", "date"]) $(`${p}-node-${field}`).value = "";
    $(`${p}-node-expiry`).value = ""; $(`${p}-node-date`).disabled = true;
    text(`${p}-node-save`, "Add node"); $(`${p}-node-cancel`).classList.toggle("hidden", true);
    nodeFormKind(p);
  }
  function editNode(p, id, node) {
    state.perms.editing[p] = id;
    $(`${p}-node-kind`).value = node.kind; nodeFormKind(p);
    $(`${p}-node-key`).value = node.kind === "inherit" ? node.group : node.kind === "prefix" || node.kind === "suffix" ? String(node.priority) : node.key;
    $(`${p}-node-value`).value = node.kind === "prefix" || node.kind === "suffix" ? node.text : node.kind === "meta" ? node.value : "";
    $(`${p}-node-grant`).value = node.value === false ? "false" : "true";
    $(`${p}-node-contexts`).value = contextText(node.contexts);
    $(`${p}-node-expiry`).value = node.expires_at ? "date" : "";
    $(`${p}-node-date`).value = node.expires_at ? localInput(node.expires_at) : ""; $(`${p}-node-date`).disabled = !node.expires_at;
    text(`${p}-node-save`, "Save node"); $(`${p}-node-cancel`).classList.toggle("hidden", false);
    permsOutcome(`${p}-node-result`, `Editing ${nodeKey(node)}. Saving replaces it, including when you change its contexts.`);
    $(`${p}-node-key`).focus?.();
  }
  async function saveNode(p) {
    let node;
    try { node = nodeFromForm(p); } catch (error) { permsOutcome(`${p}-node-result`, error.message, "error"); return; }
    const id = state.perms.editing[p], base = nodeBase(p);
    await permsMutation(`${p}-node-result`, "Saving node…", async () => {
      const applied = await request(id ? `${base}/nodes/${encodeURIComponent(id)}` : `${base}/nodes`, { method: "PUT", body: json(node) });
      state.perms.editing[p] = null;
      await reloadSubject(p);
      return appliedText(applied, `Saved ${nodeKey(node)}.`, `${nodeKey(node)} was already set exactly like that.`);
    });
  }
  async function removeNode(p, id, node) {
    if (!window.confirm(`Remove ${kindLabels[node.kind] || node.kind} ${nodeKey(node)}${node.contexts ? ` in ${contextText(node.contexts)}` : ""}?`)) return;
    const base = nodeBase(p);
    await permsMutation(`${p}-node-result`, "Removing node…", async () => {
      const applied = await request(`${base}/nodes/${encodeURIComponent(id)}`, { method: "DELETE", body: "{}" });
      await reloadSubject(p);
      return appliedText(applied, `Removed ${nodeKey(node)}.`, `${nodeKey(node)} was already gone.`);
    });
  }
  async function reloadSubject(p) {
    if (p === "perm-group") { await loadPermGroups(); return; }
    await loadPermUser(state.perms.user);
  }

  // Users.
  async function searchPermUsers() {
    const session = state.session, query = $("perm-user-query").value.trim(), group = $("perm-user-group").value;
    const params = [`limit=100`, ...(query ? [`q=${encodeURIComponent(query)}`] : []), ...(group ? [`group=${encodeURIComponent(group)}`] : [])];
    const data = await request(`/api/perms/users?${params.join("&")}`);
    if (session !== state.session) return;
    state.perms.users = Array.isArray(data.users) ? data.users : [];
    text("perm-user-count", `${state.perms.users.length}${data.truncated ? "+" : ""} found`);
    table("perm-users", state.perms.users, 5, (tr, user) => {
      tr.setAttribute("aria-current", String(user.uuid === state.perms.user));
      cell(tr, user.name || "Unknown name", user.name ? "" : "unlisted"); cell(tr, user.uuid, "address");
      cell(tr, user.online ? "Online" : "Offline", `state ${user.online ? "running" : "stopped"}`);
      cell(tr, (user.groups || []).join(", ") || "default");
      const actions = cell(tr, ""); actions.className = "action-group";
      actions.append(permsButton("Open", () => selectPermUser(user.uuid), { aria: `Open ${user.name || user.uuid}`, write: false }));
    }, query || group ? "No player matches. Players appear after they join or once a node is set for their UUID." : "No players known yet.");
    permsOutcome("perm-users-result", data.truncated ? "Showing the first 100 matches; narrow the search to see more." : "");
  }
  async function selectPermUser(user) {
    state.perms.user = user; state.perms.editing["perm-user"] = null;
    window.location.hash = "#perm-users";
    try { await loadPermUser(user); await loadCompiled(); } catch (error) { permsOutcome("perm-user-result", error.message, "error"); }
    $("perm-user-primary").focus?.();
  }
  async function loadPermUser(user) {
    const session = state.session, data = await request(`/api/perms/users/${encodeURIComponent(user)}`);
    if (session !== state.session || state.perms.user !== user) return;
    // Names resolve to a UUID; keep the UUID from now on.
    state.perms.user = data.uuid || user;
    renderPermUser(data);
  }
  function renderPermUser(user) {
    state.perms.userDetail = user;
    for (const id of ["perm-user-panel", "perm-user-nodes-panel", "perm-compiled-panel"]) $(id).classList.toggle("hidden", !user);
    if (!user) return controls();
    text("perm-user-heading", user.name || "Unknown name"); text("perm-user-uuid", user.uuid);
    $("perm-user-summary").replaceChildren();
    for (const [label, value] of [["Status", user.online ? "Online" : "Offline"], ["Primary group", `${user.effective_primary_group || "default"}${user.primary_group ? " (set explicitly)" : ""}`],
      ["Tracks", (user.tracks || []).join(", ") || "None"], ["Current contexts", contextText(user.contexts) || "None (offline or no server yet)"]]) {
      const dt = document.createElement("dt"), dd = document.createElement("dd"); dt.textContent = label; dd.textContent = value; $("perm-user-summary").append(dt, dd);
    }
    $("perm-user-primary").value = user.primary_group || "";
    state.perms.buttons = state.perms.buttons.filter((button) => !button.parentChip);
    $("perm-user-parents").replaceChildren();
    const parents = (user.nodes || []).filter(({ node }) => node.kind === "inherit");
    for (const { id, node } of parents) {
      const chip = document.createElement("span"), label = document.createElement("span");
      chip.className = "chip";
      label.textContent = `${node.value === false ? "not " : ""}${node.group}${node.contexts ? ` · ${contextText(node.contexts)}` : ""}${node.expires_at ? ` · until ${expiryText(node.expires_at)}` : ""}`;
      const remove = permsButton("×", () => removeNode("perm-user", id, node), { className: "", aria: `Remove ${user.name || user.uuid} from ${node.group}${node.contexts ? ` in ${contextText(node.contexts)}` : ""}` });
      remove.title = `Remove ${node.group}`; remove.parentChip = true;
      chip.append(label, remove); $("perm-user-parents").append(chip);
    }
    if (!parents.length) { const none = document.createElement("span"); none.className = "muted small"; none.textContent = "Only default, which everyone inherits."; $("perm-user-parents").append(none); }
    renderNodes("perm-user", user.nodes || []);
    for (const row of $("perm-users").children) row.setAttribute?.("aria-current", String(row.children?.[1]?.textContent === user.uuid));
    controls();
  }
  function compiledQuery() {
    const pairs = [];
    for (const key of ["server", "world", "dimension", "gamemode"]) {
      const value = $(`perm-compiled-${key}`).value.trim();
      if (value) pairs.push([key, value]);
    }
    for (const [key, value] of Object.entries(parseContexts($("perm-compiled-extra").value))) for (const item of [value].flat()) pairs.push([key, item]);
    return pairs;
  }
  // Rank to the subject it stands for: 0 is the player, then one rank per group.
  function rankNames(subject) {
    const names = new Map([[0, "player"]]);
    for (const entry of subject?.entries || []) if (entry.k === "group" && !names.has(entry.rank)) names.set(entry.rank, entry.key);
    return names;
  }
  async function loadCompiled() {
    const session = state.session, user = state.perms.user;
    let pairs;
    try { pairs = compiledQuery(); } catch (error) { permsOutcome("perm-trace-result", error.message, "error"); return; }
    const query = pairs.map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`).join("&");
    const data = await request(`/api/perms/users/${encodeURIComponent(user)}/compiled${query ? `?${query}` : ""}`);
    if (session !== state.session || user !== state.perms.user) return;
    const subject = data.subject || {}, meta = data.meta || {}, ranks = rankNames(subject);
    text("perm-compiled-revision", subject.revision === undefined ? "—" : `Built at revision ${subject.revision}`);
    $("perm-compiled-meta").replaceChildren();
    for (const [label, value] of [["Contexts", `${contextText(data.contexts) || "None"}${data.contexts_from === "proxy" ? " (current on the proxy)" : ""}`], ["Groups", (data.groups || []).join(", ") || "default"],
      ["Prefix", meta.prefix ?? "None"], ["Suffix", meta.suffix ?? "None"], ["Primary group", meta.primary_group || subject.primary_group || "default"],
      ["Meta", Object.entries(meta.meta || {}).map(([key, value]) => `${key}=${value}`).join(", ") || "None"]]) {
      const dt = document.createElement("dt"), dd = document.createElement("dd"); dt.textContent = label; dd.textContent = value; $("perm-compiled-meta").append(dt, dd);
    }
    const entries = [...(subject.entries || []), ...(subject.prefix || []).map((item) => ({ ...item, k: "prefix" })), ...(subject.suffix || []).map((item) => ({ ...item, k: "suffix" })), ...(subject.meta || []).map((item) => ({ ...item, k: "meta" }))];
    table("perm-compiled-entries", entries, 5, (tr, entry) => {
      const label = entry.k === "perm" ? entry.key : entry.k === "group" ? `group.${entry.key}` : entry.k === "meta" ? `meta ${entry.key}` : `${entry.k} ${entry.p}`;
      cell(tr, label, "address");
      cell(tr, entry.k === "perm" || entry.k === "group" ? entry.v === false ? "false" : "true" : entry.k === "meta" ? entry.v : entry.t);
      cell(tr, contextText(entry.ctx) || "Everywhere");
      cell(tr, `${ranks.get(entry.rank) || "group"} · rank ${entry.rank}`);
      cell(tr, expiryText(entry.exp));
    }, "Nothing compiled: this player gets only what backends default to.");
  }
  async function tracePermission() {
    const node = $("perm-trace-node").value.trim();
    if (!node) return;
    let pairs;
    try { pairs = compiledQuery(); } catch (error) { permsOutcome("perm-trace-result", error.message, "error"); return; }
    const body = { user: state.perms.user, node };
    if (pairs.length) body.contexts = pairs.reduce((contexts, [key, value]) => ({ ...contexts, [key]: Object.hasOwn(contexts, key) ? [contexts[key], value].flat() : value }), {});
    try {
      const trace = await request("/api/perms/trace", { method: "POST", body: json(body) });
      const names = new Map((trace.ranks || []).map((rank) => [rank.rank, rank.subject]));
      permsOutcome("perm-trace-result", trace.result === true ? `${trace.node} is granted.` : trace.result === false ? `${trace.node} is denied.` : `${trace.node} is not set; the backend’s default for it applies.`, trace.result === null || trace.result === undefined ? "warning" : trace.result ? "success" : "error");
      const rows = [...(trace.winner ? [{ ...trace.winner, won: true }] : []), ...(trace.losers || [])];
      $("perm-trace-table").classList.toggle("hidden", !rows.length);
      table("perm-trace-rows", rows, 5, (tr, candidate) => {
        const entry = candidate.entry || {};
        cell(tr, candidate.won ? "Wins" : { lower_precedence: "Lost to a stronger entry", context_mismatch: "Contexts not active", expired: "Expired" }[candidate.outcome] || candidate.outcome);
        cell(tr, entry.k === "group" ? `group.${entry.key}` : entry.key, "address"); cell(tr, entry.v === false ? "false" : "true");
        cell(tr, contextText(entry.ctx) || "Everywhere"); cell(tr, `${names.get(entry.rank) || "group"} · rank ${entry.rank}`);
      }, "No entry matches.");
    } catch (error) { permsOutcome("perm-trace-result", error.message, "error"); }
  }
  async function moveOnTrack(direction) {
    const track = $("perm-user-track").value;
    if (!track) { permsOutcome("perm-user-result", "Create a track under Groups first.", "error"); return; }
    let contexts;
    try { contexts = parseContexts($("perm-user-track-contexts").value); } catch (error) { permsOutcome("perm-user-result", error.message, "error"); return; }
    const body = { user: state.perms.user };
    if (Object.keys(contexts).length) body.contexts = contexts;
    await permsMutation("perm-user-result", `${direction === "promote" ? "Promoting" : "Demoting"} on ${track}…`, async () => {
      const applied = await request(`/api/perms/tracks/${encodeURIComponent(track)}/${direction}`, { method: "POST", body: json(body) });
      await loadPermUser(state.perms.user); await loadCompiled();
      return appliedText(applied, `${direction === "promote" ? "Promoted" : "Demoted"} on ${track}.`, "Nothing changed.");
    });
  }

  // Registered nodes and holders.
  async function loadRegistered() {
    const session = state.session, data = await request("/api/perms/registered");
    if (session !== state.session) return;
    state.perms.registered = Array.isArray(data.registered) ? data.registered : [];
    renderRegistered();
  }
  function renderRegistered() {
    const filter = $("perm-registered-filter").value.trim().toLowerCase();
    const owner = (item) => `${item.owner}${item.plugin ? ` · ${item.plugin}` : ""}`;
    const items = state.perms.registered.filter((item) => !filter || [item.node, item.description, owner(item)].some((value) => String(value || "").toLowerCase().includes(filter)));
    items.sort((a, b) => owner(a).localeCompare(owner(b)) || a.node.localeCompare(b.node));
    text("perm-registered-count", `${state.perms.registered.length} registered`);
    const body = $("perm-registered"); body.replaceChildren();
    let heading = null;
    for (const item of items) {
      if (owner(item) !== heading) {
        heading = owner(item);
        const tr = document.createElement("tr"), th = document.createElement("th");
        th.colSpan = 4; th.scope = "colgroup"; th.className = "group-row"; th.textContent = heading; tr.append(th); body.append(tr);
      }
      const tr = document.createElement("tr");
      cell(tr, item.node, "address"); cell(tr, String(item.default)); cell(tr, item.description || "—");
      const actions = cell(tr, ""); actions.className = "action-group";
      actions.append(permsButton("Holders", () => findHolders(item.node), { aria: `Who holds ${item.node}`, write: false }));
      body.append(tr);
    }
    if (!items.length) {
      const tr = document.createElement("tr"), td = document.createElement("td");
      td.colSpan = 4; td.className = "empty";
      td.textContent = filter ? "No registered node matches." : "No backend has registered nodes yet. They appear once a backend plugin with the Rift permissions adapter connects.";
      tr.append(td); body.append(tr);
    }
  }
  async function findHolders(node) {
    $("perm-holders-node").value = node;
    const session = state.session, data = await request(`/api/perms/search?kind=permission&limit=200&node=${encodeURIComponent(node)}`).catch((error) => { permsOutcome("perm-holders-result", error.message, "error"); return null; });
    if (!data || session !== state.session) return;
    const holders = Array.isArray(data.holders) ? data.holders : [];
    text("perm-holder-count", `${holders.length}${data.truncated ? "+" : ""} for ${node}`);
    table("perm-holders", holders, 6, (tr, holder) => {
      const user = holder.subject?.user, group = holder.subject?.group;
      cell(tr, user ? `${holder.name || "Unknown name"} · ${user}` : `group ${group}`, user ? "address" : "");
      cell(tr, nodeValue(holder.node || {})); cell(tr, contextText(holder.node?.contexts) || "Everywhere");
      cell(tr, expiryText(holder.node?.expires_at)); cell(tr, holder.node?.source || "store");
      const actions = cell(tr, ""); actions.className = "action-group";
      actions.append(permsButton("Open", () => user ? selectPermUser(user) : selectPermGroup(group), { aria: `Open ${user || group}`, write: false }));
    }, "Nobody sets this node directly. Wildcards such as prefix.* can still grant it; check a player under Users to see.");
    permsOutcome("perm-holders-result", "");
  }

  // Activity: the change log with a diff per revision.
  async function loadActivity() {
    const session = state.session, params = ["limit=100"];
    const actor = $("perm-activity-actor").value.trim(), subject = $("perm-activity-subject").value.trim(), since = $("perm-activity-since").value.trim();
    if (actor) params.push(`actor=${encodeURIComponent(actor)}`);
    if (subject) params.push(`subject=${encodeURIComponent(subject)}`);
    if (since) params.push(`since=${encodeURIComponent(since)}`);
    const data = await request(`/api/perms/audit?${params.join("&")}`);
    if (session !== state.session) return;
    state.perms.events = (Array.isArray(data.events) ? data.events : []).slice().reverse();
    text("perm-activity-count", `Revision ${data.revision ?? "—"}`);
    renderActivity();
    permsOutcome("perm-activity-result", state.perms.events.length >= 100 ? "Showing 100 changes. Filter by actor, subject or revision to see others." : "");
  }
  function renderActivity() {
    const body = $("perm-activity"); body.replaceChildren();
    for (const event of state.perms.events) {
      const tr = document.createElement("tr"), ops = event.ops || [];
      cell(tr, String(event.revision), "metric-value"); cell(tr, new Date(number(event.at) * 1000).toLocaleString()); cell(tr, event.actor, "address");
      const subjects = [...new Set(ops.map((op) => op.op === "node" ? subjectLabel(op.subject) : op.op === "primary_group" ? `user:${op.user}` : `${op.op}:${op.name}`))];
      cell(tr, `${ops.length}${event.ops_truncated ? "+" : ""} ${ops.length === 1 ? "change" : "changes"} · ${subjects.slice(0, 3).join(", ")}${subjects.length > 3 ? ` and ${subjects.length - 3} more` : ""}`);
      cell(tr, event.reason || "—");
      const actions = cell(tr, ""); actions.className = "action-group";
      const open = state.perms.expanded.has(event.revision);
      const toggle = permsButton(open ? "Hide diff" : "Show diff", () => {
        if (state.perms.expanded.has(event.revision)) state.perms.expanded.delete(event.revision); else state.perms.expanded.add(event.revision);
        renderActivity();
      }, { aria: `${open ? "Hide" : "Show"} the changes of revision ${event.revision}`, write: false });
      toggle.setAttribute("aria-expanded", String(open));
      actions.append(toggle); body.append(tr);
      if (!open) continue;
      const detail = document.createElement("tr"), td = document.createElement("td");
      td.colSpan = 6; td.className = "diff-cell";
      for (const op of ops) {
        const { title, lines } = opDiff(op), heading = document.createElement("p"), pre = document.createElement("pre");
        heading.className = "diff-title"; heading.textContent = title; pre.className = "diff";
        for (const line of lines) {
          const span = document.createElement("span");
          span.className = line.sign === "+" ? "diff-add" : line.sign === "-" ? "diff-del" : "diff-same";
          span.textContent = `${line.sign} ${line.text}\n`; pre.append(span);
        }
        td.append(heading, pre);
      }
      if (event.ops_truncated) { const note = document.createElement("p"); note.className = "muted small"; note.textContent = "This change had more effects than one event carries."; td.append(note); }
      detail.append(td); body.append(detail);
    }
    if (!state.perms.events.length) {
      const tr = document.createElement("tr"), td = document.createElement("td");
      td.colSpan = 6; td.className = "empty"; td.textContent = "No permission changes match."; tr.append(td); body.append(tr);
    }
  }

  // Permission view events.
  $("perm-groups-refresh").addEventListener("click", () => loadPermGroups().catch((error) => permsOutcome("perm-groups-result", error.message, "error")));
  $("perm-group-create-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const name = $("perm-group-create-name").value.trim(), weight = $("perm-group-create-weight").value.trim(), display = $("perm-group-create-display").value.trim();
    if (!/^[A-Za-z0-9_.-]{1,64}$/.test(name)) { permsOutcome("perm-groups-result", "Enter a group name using letters, digits, dots, underscores or hyphens.", "error"); return; }
    if (weight && !/^-?\d{1,10}$/.test(weight)) { permsOutcome("perm-groups-result", "The weight must be a whole number.", "error"); return; }
    const body = { name, weight: number(weight) };
    if (display) body.display_name = display;
    permsMutation("perm-groups-result", `Creating ${name}…`, async () => {
      const created = await request("/api/perms/groups", { method: "POST", body: json(body) });
      for (const id of ["perm-group-create-name", "perm-group-create-display"]) $(id).value = "";
      $("perm-group-create-weight").value = "0";
      state.perms.group = created.name || name.toLowerCase();
      await loadPermGroups();
      return `Created ${created.name || name}. Add nodes to it below.`;
    });
  });
  $("perm-group-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const group = state.perms.groupDetail, weight = $("perm-group-weight").value.trim(), display = $("perm-group-display").value.trim();
    if (!group) return;
    if (!/^-?\d{1,10}$/.test(weight)) { permsOutcome("perm-group-result", "The weight must be a whole number.", "error"); return; }
    permsMutation("perm-group-result", `Saving ${group.name}…`, async () => {
      const applied = await request(`/api/perms/groups/${encodeURIComponent(group.name)}`, { method: "PATCH", body: json({ weight: number(weight), display_name: display || null }) });
      await loadPermGroups();
      return appliedText(applied, `Saved ${group.name}.`, "Nothing changed.");
    });
  });
  $("perm-group-delete").addEventListener("click", () => {
    const group = state.perms.groupDetail;
    if (!group || !window.confirm(`Delete the group ${group.name} and its nodes? Players and groups that inherit it keep the parent, which grants nothing until the group exists again.`)) return;
    permsMutation("perm-groups-result", `Deleting ${group.name}…`, async () => {
      await request(`/api/perms/groups/${encodeURIComponent(group.name)}`, { method: "DELETE", body: "{}" });
      state.perms.group = ""; await loadPermGroups();
      return `Deleted ${group.name}.`;
    });
  });
  for (const p of ["perm-group", "perm-user"]) {
    $(`${p}-node-kind`).addEventListener("change", () => nodeFormKind(p));
    $(`${p}-node-expiry`).addEventListener("change", () => { $(`${p}-node-date`).disabled = $(`${p}-node-expiry`).value !== "date"; });
    $(`${p}-node-cancel`).addEventListener("click", () => { resetNodeForm(p); permsOutcome(`${p}-node-result`, ""); });
    $(`${p}-node-form`).addEventListener("submit", (event) => { event.preventDefault(); saveNode(p); });
  }
  $("perm-track-select").addEventListener("change", () => {
    const chosen = $("perm-track-select").value;
    $("perm-track-select").value = $("perm-track-name").value && state.perms.tracks.some((track) => track.name === $("perm-track-name").value) ? $("perm-track-name").value : "";
    selectPermTrack(chosen);
  });
  $("perm-track-name").addEventListener("input", () => { state.perms.trackDirty = true; });
  $("perm-track-add").addEventListener("click", () => {
    const group = $("perm-track-add-group").value;
    if (!group || state.perms.trackGroups.includes(group)) return;
    state.perms.trackGroups.push(group); state.perms.trackDirty = true; renderTrackGroups();
  });
  $("perm-track-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const name = $("perm-track-name").value.trim(), groups = [...state.perms.trackGroups];
    if (!/^[A-Za-z0-9_.-]{1,64}$/.test(name)) { permsOutcome("perm-track-result", "Enter a track name using letters, digits, dots, underscores or hyphens.", "error"); return; }
    if (!groups.length) { permsOutcome("perm-track-result", "Add at least one group to the track.", "error"); return; }
    permsMutation("perm-track-result", `Saving track ${name}…`, async () => {
      const applied = await request(`/api/perms/tracks/${encodeURIComponent(name)}`, { method: "PUT", body: json({ groups }) });
      state.perms.trackDirty = false; await loadPermGroups(); selectPermTrack(name.toLowerCase(), { focus: false });
      return appliedText(applied, `Saved track ${name}: ${groups.join(" → ")}.`, "The track already had these groups.");
    });
  });
  $("perm-track-delete").addEventListener("click", () => {
    const name = $("perm-track-select").value;
    if (!name || !window.confirm(`Delete the track ${name}? Players keep their groups.`)) return;
    permsMutation("perm-track-result", `Deleting track ${name}…`, async () => {
      await request(`/api/perms/tracks/${encodeURIComponent(name)}`, { method: "DELETE", body: "{}" });
      state.perms.trackDirty = false; await loadPermGroups(); selectPermTrack("", { focus: false });
      return `Deleted track ${name}.`;
    });
  });
  $("perm-user-search-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const query = $("perm-user-query").value.trim(), compact = query.replaceAll("-", "").toLowerCase();
    searchPermUsers().then(() => {
      // A full UUID opens the player directly, even one the store has not seen.
      if (/^[0-9a-f]{32}$/.test(compact) && !state.perms.users.some((user) => user.uuid === compact)) selectPermUser(compact);
    }).catch((error) => permsOutcome("perm-users-result", error.message, "error"));
  });
  $("perm-user-primary-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const group = $("perm-user-primary").value || null;
    permsMutation("perm-user-result", "Saving primary group…", async () => {
      const applied = await request(`/api/perms/users/${encodeURIComponent(state.perms.user)}`, { method: "PATCH", body: json({ primary_group: group }) });
      await loadPermUser(state.perms.user); await loadCompiled();
      return appliedText(applied, group ? `Primary group set to ${group}.` : "The primary group is automatic again.", "Nothing changed.");
    });
  });
  $("perm-user-parent-expiry").addEventListener("change", () => { $("perm-user-parent-date").disabled = $("perm-user-parent-expiry").value !== "date"; });
  $("perm-user-parent-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const group = $("perm-user-parent-group").value;
    if (!group) { permsOutcome("perm-user-result", "Create a group first.", "error"); return; }
    const node = { kind: "inherit", group, value: true };
    try {
      const contexts = parseContexts($("perm-user-parent-contexts").value);
      if (Object.keys(contexts).length) node.contexts = contexts;
      const expires = expiryValue("perm-user-parent-expiry", "perm-user-parent-date");
      if (expires) node.expires_at = expires;
    } catch (error) { permsOutcome("perm-user-result", error.message, "error"); return; }
    permsMutation("perm-user-result", `Adding ${group}…`, async () => {
      const applied = await request(`/api/perms/users/${encodeURIComponent(state.perms.user)}/nodes`, { method: "PUT", body: json(node) });
      $("perm-user-parent-contexts").value = ""; $("perm-user-parent-expiry").value = ""; $("perm-user-parent-date").disabled = true;
      await loadPermUser(state.perms.user); await loadCompiled();
      return appliedText(applied, `Added ${group}. It applies to the next permission check.`, `Already in ${group} exactly like that.`);
    });
  });
  $("perm-user-promote").addEventListener("click", () => moveOnTrack("promote"));
  $("perm-user-demote").addEventListener("click", () => moveOnTrack("demote"));
  $("perm-compiled-form").addEventListener("submit", (event) => { event.preventDefault(); loadCompiled().catch((error) => permsOutcome("perm-trace-result", error.message, "error")); });
  $("perm-trace-form").addEventListener("submit", (event) => { event.preventDefault(); tracePermission(); });
  $("perm-nodes-refresh").addEventListener("click", () => loadRegistered().catch((error) => permsOutcome("perm-holders-result", error.message, "error")));
  $("perm-registered-filter").addEventListener("input", () => renderRegistered());
  $("perm-registered-form").addEventListener("submit", (event) => event.preventDefault());
  $("perm-holders-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const node = $("perm-holders-node").value.trim();
    if (node) findHolders(node);
  });
  $("perm-activity-refresh").addEventListener("click", () => loadActivity().catch((error) => permsOutcome("perm-activity-result", error.message, "error")));
  $("perm-activity-form").addEventListener("submit", (event) => { event.preventDefault(); loadActivity().catch((error) => permsOutcome("perm-activity-result", error.message, "error")); });
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
  const pluginModes = { script: "Configuration script", persistent: "Persistent", remote: "Bus responders" };
  async function loadPlugins() {
    const session = state.session, data = await request("/api/plugins");
    if (session !== state.session) return;
    const plugins = Array.isArray(data.plugins) ? data.plugins : [];
    state.pluginButtons = [];
    table("plugins", plugins, 8, (tr, plugin) => {
      cell(tr, plugin.version ? `${plugin.name} ${plugin.version}` : plugin.name);
      cell(tr, pluginModes[plugin.mode] || plugin.mode);
      cell(tr, plugin.engine || "—");
      cell(tr, plugin.status, `state ${plugin.status === "active" ? "running" : plugin.status}`);
      cell(tr, plugin.restartable ? count(plugin.restarts) : "—", "metric-value");
      cell(tr, plugin.restartable ? count(plugin.calls) : "—", "metric-value");
      cell(tr, plugin.last_error || "—");
      const actions = cell(tr, "");
      if (!plugin.restartable) return;
      const button = document.createElement("button");
      button.type = "button"; button.className = "button subtle"; button.textContent = "Restart";
      button.setAttribute("aria-label", `Restart ${plugin.name}`);
      state.pluginButtons.push(button);
      button.addEventListener("click", () => { if (!button.disabled) restartPlugin(plugin.name); });
      actions.append(button);
    }, "No plugins configured. Install one under plugins/ and enable it with rift.plugin.");
    // Only persistent plugins keep a log tail.
    const persistent = plugins.filter((plugin) => plugin.mode === "persistent").map((plugin) => plugin.name);
    const select = $("plugin-log-select"), selected = state.pluginLog;
    select.replaceChildren();
    for (const name of ["", ...persistent]) {
      const option = document.createElement("option"); option.value = name; option.textContent = name || "Choose a plugin"; select.append(option);
    }
    select.value = persistent.includes(selected) ? selected : "";
    if (select.value !== state.pluginLog) { state.pluginLog = ""; text("plugin-log-output", "Choose a plugin to view its last 200 log lines."); }
    controls();
  }
  async function pollPluginLog() {
    if (state.pluginLogPolling || !state.pluginLog || !allowed("logs")) return;
    const session = state.session, plugin = state.pluginLog;
    state.pluginLogPolling = true;
    try {
      const data = await request(`/api/plugins/${encodeURIComponent(plugin)}/logs`);
      if (session !== state.session || plugin !== state.pluginLog) return;
      const lines = (Array.isArray(data.lines) ? data.lines : []).map((line) => `${new Date(number(line.timestamp_unix_ms)).toLocaleTimeString()}  ${String(line.level || "info").padEnd(5)}  ${line.message}`);
      text("plugin-log-output", lines.length ? lines.join("\n") : "No log lines yet.");
      if ($("plugin-log-follow").checked) $("plugin-log-output").scrollTop = $("plugin-log-output").scrollHeight;
    } catch (error) { if (session === state.session && plugin === state.pluginLog) text("plugin-result", error.message); }
    finally { state.pluginLogPolling = false; }
  }
  async function restartPlugin(name) {
    if (state.busy || !allowed("plugins")) return;
    if (!window.confirm(`Restart ${name}? Rift stops it and starts a fresh instance; calls meanwhile wait for it or time out under its manifest.`)) return;
    state.busy = true; controls();
    text("plugin-result", `Restarting ${name}…`);
    $("plugin-result").className = "operation-result";
    try {
      await request(`/api/plugins/${encodeURIComponent(name)}/restart`, { method: "POST", body: "{}" });
      text("plugin-result", `Restart accepted for ${name}. Its state updates below.`);
      $("plugin-result").className = "operation-result success";
      await loadPlugins();
      if (state.pluginLog === name) await pollPluginLog();
    } catch (error) {
      if (error.status === 401 || error.status === 403) $("auth-panel").open = true;
      text("plugin-result", error.message || "The restart request failed.");
      $("plugin-result").className = "operation-result error";
    } finally { state.busy = false; controls(); }
  }
  function wakeLabel(server) {
    if (server.group_suspended) return `Paused · group ${server.group} stopped; start the group to resume`;
    const label = baseWakeLabel(server);
    return server.group_mode === "dev" ? `${label} · dev mode` : label;
  }
  function baseWakeLabel(server) {
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
  // The status badges of a service group row, most important first.
  function groupStatus(group) {
    const draining = number(group.summary?.draining), badges = [];
    if (group.suspended) badges.push(draining ? [`Stopping · ${count(draining)} draining`, "warning"] : ["Stopped", ""]);
    if (group.mode === "dev") badges.push(["Dev mode", "accent"]);
    if (!badges.length) badges.push(["Active", "good"]);
    return badges;
  }
  function instanceSummary(group) {
    const names = group.instances || [], summary = group.summary;
    if (!names.length) return "None created";
    if (!summary) return names.join(", ");
    const parts = [["running", "running"], ["starting", "starting"], ["stopping", "stopping"], ["stopped", "stopped"], ["failed", "failed"], ["unknown", "unreachable"]]
      .filter(([key]) => number(summary[key])).map(([key, label]) => `${count(summary[key])} ${label}`);
    return [...parts, `${count(summary.players)} ${number(summary.players) === 1 ? "player" : "players"}`].join(" · ");
  }
  function groupOutcome(group, action, outcome) {
    if (action === "mode") return outcome.mode === "dev"
      ? `${group} is in dev mode. Only players with ${outcome.dev_permission} can join and scaling is paused; players already connected stay.`
      : `${group} is active again. Everyone can join${outcome.suspended ? " once the group is started" : " and scaling resumes"}.`;
    const tally = new Map(), failures = [];
    for (const instance of outcome.instances || []) {
      tally.set(instance.result, (tally.get(instance.result) || 0) + 1);
      if (instance.result === "failed") failures.push(`${instance.name}: ${instance.message}`);
    }
    const labels = { starting: "starting", running: "already running", stopping: "stopping", stopped: "already stopped", draining: "draining until their players leave", failed: "failed" };
    const parts = [...tally].map(([result, total]) => `${total} ${labels[result] || result}`);
    const counted = parts.length ? ` ${parts.join(", ")}.` : " It has no instances yet.";
    const lead = action === "start"
      ? `Started ${group}.${counted} Scaling resumes${outcome.mode === "dev" ? ", except while the group is in dev mode" : ""}.`
      : `Stopped ${group}.${counted} New players are kept out and scaling is paused until you start the group.`;
    return failures.length ? `${lead} ${failures.join("; ")}` : lead;
  }
  async function groupOperation(group, action, mode) {
    if (state.busy) return;
    state.busy = true; controls();
    const name = group.name, path = `/api/groups/${encodeURIComponent(name)}/${action}`;
    text("group-result", action === "mode" ? `Switching ${name} to ${mode === "dev" ? "dev mode" : "active"}…` : `${action === "start" ? "Starting" : "Stopping"} ${name}…`);
    $("group-result").className = "operation-result";
    try {
      const outcome = await request(path, action === "mode" ? { method: "PUT", body: JSON.stringify({ mode }) } : { method: "POST", body: "{}" });
      const failed = (outcome.instances || []).some((instance) => instance.result === "failed");
      text("group-result", groupOutcome(name, action, outcome));
      $("group-result").className = `operation-result ${failed ? "warning" : "success"}`;
      await refreshStatus({ syncSource: false });
    } catch (error) {
      if (error.status === 401 || error.status === 403) $("auth-panel").open = true;
      text("group-result", error.message || "The group request failed.");
      $("group-result").className = "operation-result error";
    } finally { state.busy = false; controls(); }
  }
  async function editGroup(name) {
    if (state.definitionDirty && !window.confirm("Discard unsaved definition fields?")) return;
    window.location.hash = "#definitions";
    try {
      if ($("definition-section").value !== "service_groups") { $("definition-section").value = "service_groups"; await loadDefinitions(); }
      $("definition-select").value = name; renderDefinition();
    } catch (error) { text("definition-result", error.message); }
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
  $("servers-group").addEventListener("change", () => { renderServers(); controls(); });
  $("source").addEventListener("input", controls);
  $("source").addEventListener("keydown", (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); validate(); }
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") { event.preventDefault(); save(); }
  });
  window.addEventListener("beforeunload", (event) => {
    if (dirty() || state.definitionDirty || settingsDirty() || state.perms.trackDirty) { event.preventDefault(); event.returnValue = ""; }
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
    state.settingsDirty = { serverList: false, tabList: false }; state.perms = permsState();
    $("source").value = ""; $("console-server").replaceChildren(); $("definition-fields").replaceChildren();
    for (const id of ["console-output", "config-path", "revision", "deployment-result", "definition-result", "console-result", "extension-output"]) text(id, "");
    for (const id of ["deployments", "audit-records", "managed-servers", "service-groups", "backends", "routes", "server-list-hosts", "templates", "files-entries",
      "perm-groups", "perm-tracks", "perm-users", "perm-registered", "perm-holders", "perm-activity", "perm-group-nodes", "perm-user-nodes", "perm-compiled-entries", "perm-trace-rows"]) $(id).replaceChildren();
    state.templateButtons = []; state.files = { root: "templates", path: "", editing: null, baseline: "", buttons: [] };
    for (const id of ["template-result", "files-result", "plugin-result"]) text(id, "");
    state.pluginButtons = []; state.pluginLog = ""; $("plugins").replaceChildren(); $("plugin-log-select").replaceChildren();
    text("plugin-log-output", "Choose a plugin to view its last 200 log lines.");
    for (const id of ["server-list-result", "tab-list-result", "perm-groups-result", "perm-group-result", "perm-track-result", "perm-users-result", "perm-user-result",
      "perm-group-node-result", "perm-user-node-result", "perm-trace-result", "perm-holders-result", "perm-activity-result"]) text(id, "");
    for (const id of ["perm-group-panel", "perm-group-nodes-panel", "perm-user-panel", "perm-user-nodes-panel", "perm-compiled-panel"]) $(id).classList.toggle("hidden", true);
    state.serverButtons = []; return true;
  }
  $("refresh-status").addEventListener("click", () => refreshStatus());
  $("console-server").addEventListener("change", () => {
    state.logServer = $("console-server").value; state.logCursor = null;
    text("console-output", ""); text("console-result", ""); controls(); pollLogs({ reset: true });
  });
  $("console-refresh").addEventListener("click", () => pollLogs());
  $("plugins-refresh").addEventListener("click", () => loadPlugins().catch((error) => text("plugin-result", error.message)));
  $("plugin-log-select").addEventListener("change", () => {
    state.pluginLog = $("plugin-log-select").value;
    text("plugin-log-output", state.pluginLog ? "" : "Choose a plugin to view its last 200 log lines."); text("plugin-result", "");
    pollPluginLog();
  });
  $("plugin-log-refresh").addEventListener("click", () => pollPluginLog());
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
  for (const [form, key] of [["server-list-form", "serverList"], ["tab-list-form", "tabList"]]) {
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
  $("server-list-refresh").addEventListener("click", () => refreshSettings("serverList", "server-list-result"));
  $("tab-list-refresh").addEventListener("click", () => refreshSettings("tabList", "tab-list-result"));
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

  setInterval(() => {
    if (document.hidden || state.busy || !allowed("read")) return;
    refreshStatus();
    if ($("view-plugins").hidden === false && pluginsVisible()) loadPlugins().catch((error) => text("plugin-result", error.message));
  }, 5000);
  setInterval(() => {
    if (document.hidden || state.busy) return;
    if ($("console-follow").checked) pollLogs();
    if ($("view-plugins").hidden === false && $("plugin-log-follow").checked) pollPluginLog();
  }, 1000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden && !state.busy) refreshStatus(); });
  controls(); connect();
})();
