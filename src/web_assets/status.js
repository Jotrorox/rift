(() => {
  "use strict";
  const $ = (id) => document.getElementById(id);
  const text = (id, value) => { $(id).textContent = value; };
  const hide = (id, hidden) => { $(id).classList.toggle("hidden", hidden); };
  const number = (value) => Number.isFinite(Number(value)) ? Number(value) : 0;
  const count = (value) => number(value).toLocaleString();
  const plural = (amount, one, many = `${one}s`) => `${count(amount)} ${number(amount) === 1 ? one : many}`;
  const DEFAULT_REFRESH = 5000, MIN_REFRESH = 1000, MAX_REFRESH = 300000;
  let pending = false, timer = null, interval = DEFAULT_REFRESH, appliedLinks = "";
  function bytes(value) {
    let amount = number(value), unit = 0;
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    while (amount >= 1024 && unit < units.length - 1) { amount /= 1024; unit += 1; }
    return `${amount.toFixed(unit && amount < 100 ? 1 : 0)} ${units[unit]}`;
  }
  function uptime(value) {
    const seconds = Math.floor(number(value));
    if (seconds >= 86400) return `${Math.floor(seconds / 86400)}d ${Math.floor(seconds % 86400 / 3600)}h`;
    if (seconds >= 3600) return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`;
    if (seconds >= 60) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
    return `${seconds}s`;
  }
  function cell(row, value, className = "") {
    const td = document.createElement("td");
    td.textContent = value ?? "—"; td.className = className; row.append(td);
    return td;
  }
  function table(id, values, width, render) {
    $(id).replaceChildren();
    for (const value of values) {
      const tr = document.createElement("tr"); render(tr, value); $(id).append(tr);
    }
    if (!values.length) {
      const tr = document.createElement("tr"), td = cell(tr, "None configured", "empty");
      td.colSpan = width; $(id).append(tr);
    }
  }
  // The server validates these too; the page re-checks before handing them to the DOM.
  const safeHref = (href) => typeof href === "string" && /^(https?:\/\/[^\s/]|\/(?![/\\]))/.test(href);
  const safeColor = (color) => typeof color === "string" && /^#[0-9a-f]{6}$/i.test(color);
  function applyPage(page, publicView) {
    page = page && typeof page === "object" ? page : {};
    const title = typeof page.title === "string" && page.title ? page.title : "Rift";
    text("brand-name", title);
    document.title = `Network status · ${title}`;
    const style = document.documentElement?.style;
    if (style) {
      for (const [name, suffix] of [["--accent", ""], ["--accent-hover", ""], ["--accent-soft", "1f"]]) {
        if (safeColor(page.accent)) style.setProperty(name, `${page.accent}${suffix}`); else style.removeProperty(name);
      }
    }
    text("announcement", typeof page.notice === "string" ? page.notice : "");
    hide("announcement", !page.notice);
    const panels = page.panels || {};
    hide("summary", panels.summary === false);
    hide("backends-panel", panels.backends === false);
    hide("listeners-panel", publicView || panels.listeners === false);
    hide("metrics-panel", publicView || panels.metrics === false);
    $("panels").classList.toggle("single", $("backends-panel").className.includes("hidden") || $("listeners-panel").className.includes("hidden"));
    const links = Array.isArray(page.links) ? page.links.filter((link) => link && typeof link.label === "string" && safeHref(link.href)) : [];
    const signature = JSON.stringify(links.map((link) => [link.label, link.href]));
    if (signature !== appliedLinks) {
      appliedLinks = signature;
      $("links").replaceChildren(...links.map((link) => {
        const a = document.createElement("a");
        a.className = "text-link"; a.textContent = link.label; a.href = link.href;
        if (/^https?:/.test(link.href)) a.rel = "noopener noreferrer";
        return a;
      }));
    }
    hide("links", !links.length);
    const requested = Math.round(number(page.refresh_ms));
    interval = requested ? Math.min(MAX_REFRESH, Math.max(MIN_REFRESH, requested)) : DEFAULT_REFRESH;
    const seconds = interval / 1000;
    text("refresh-interval", `Refreshes every ${seconds === 1 ? "second" : `${Number.isInteger(seconds) ? seconds : seconds.toFixed(1)} seconds`}`);
  }
  function healthLabel(backend, checks, publicView) {
    if (!checks) return publicView ? ["unknown", "Unknown"] : ["unknown", "Checks disabled"];
    if (backend.healthy === false) return ["bad", publicView ? "Offline" : "Unhealthy"];
    if (backend.healthy === true) return ["", publicView ? "Online" : "Healthy"];
    return ["unknown", "Unknown"];
  }
  function render(status) {
    const publicView = status.exposure === "public";
    const metrics = status.metrics || {}, backends = Array.isArray(status.backends) ? status.backends : [];
    const checks = status.health_checks_enabled !== false;
    const healthy = checks ? backends.filter((backend) => backend.healthy === true).length : 0;
    const unhealthy = checks ? backends.filter((backend) => backend.healthy === false).length : 0;
    const unknown = backends.length - healthy - unhealthy;
    const players = status.players ?? metrics.players;
    const noun = publicView ? "server" : "backend";
    applyPage(status.page, publicView);
    text("version", status.version ? ` · v${status.version}` : "");
    text("metric-players", count(players));
    text("metric-connections", publicView
      ? `Across ${plural(backends.length, "server")}`
      : `${plural(metrics.active, "active connection")} · ${count(metrics.accepted)} accepted`);
    text("metric-backends-label", publicView ? "Servers online" : "Backend health");
    text("metric-backends", unknown === backends.length && backends.length ? "Unmonitored" : `${healthy} / ${backends.length}`);
    text("metric-health", unknown ? `${unknown} without a health result` : publicView ? "Online servers / total" : "Healthy backends / total");
    hide("traffic-stat", publicView);
    text("metric-traffic", bytes(number(metrics.sent) + number(metrics.received)));
    text("metric-directions", `${bytes(metrics.sent)} sent · ${bytes(metrics.received)} received`);
    text("metric-uptime", uptime(status.uptime_seconds));
    text("metric-errors", publicView ? "Since the proxy last started" : `${count(metrics.errors)} errors · ${count(metrics.reloads)} reloads`);
    const allDown = unhealthy && unhealthy === backends.length;
    verdict(unhealthy ? "bad" : !checks || unknown ? "warning" : "good",
      publicView
        ? allDown ? "Network offline"
          : unhealthy ? `${unhealthy} of ${plural(backends.length, noun)} offline`
            : !backends.length || !checks || unknown === backends.length ? "Network is running"
              : unknown ? "Network is running, some servers unknown"
                : "All servers online"
        : unhealthy ? `${unhealthy} of ${plural(backends.length, noun)} unhealthy`
          : !backends.length ? "Proxy is running"
            : !checks || unknown === backends.length ? "Proxy is running, health unmonitored"
              : unknown ? "Proxy is running, some health unknown"
                : `All ${backends.length === 1 ? "backends" : `${backends.length} backends`} healthy`);
    text("status-summary", publicView
      ? allDown ? "No servers are accepting players right now." : unhealthy ? `${plural(unhealthy, "server")} ${unhealthy === 1 ? "is" : "are"} offline. Players can still join the others.` : !checks ? "The network is reachable. Server health is not monitored." : unknown ? "The network is reachable. Some servers have not reported yet." : "Players can join every server."
      : unhealthy ? `${unhealthy} backend${unhealthy === 1 ? " is" : "s are"} reporting unhealthy. The proxy status service is reachable.` : !checks ? "Proxy is reachable. Backend health checks are disabled." : unknown ? "Proxy is reachable. Some backends do not have a health result." : "Proxy is reachable. All monitored backends are healthy.");
    text("last-update", `Updated ${new Date().toLocaleTimeString()}`);
    hide("metrics-link", !status.services?.status?.metrics);
    text("backends-heading", publicView ? "Servers" : "Backends");
    hide("backend-address-heading", publicView);
    table("listeners", status.listeners || [], 2, (tr, listener) => { cell(tr, listener.name); cell(tr, listener.address, "address"); });
    table("backends", backends, publicView ? 3 : 4, (tr, backend) => {
      const name = cell(tr, backend.display || backend.name);
      if (!publicView && backend.display && backend.display !== backend.name) name.title = backend.name;
      if (!publicView) cell(tr, backend.address, "address");
      cell(tr, count(backend.players), "metric-value");
      const [className, label] = healthLabel(backend, checks, publicView);
      const span = document.createElement("span");
      span.className = `health ${className}`; span.textContent = label;
      cell(tr, "").append(span);
    });
    table("metrics", Object.entries(metrics).filter(([, value]) => typeof value === "number"), 2, (tr, [key, value]) => {
      cell(tr, key.replaceAll("_", " ")); cell(tr, count(value), "metric-value");
    });
  }
  function verdict(kind, title) {
    $("verdict").className = `verdict ${kind}`;
    text("status-verdict", title);
  }
  function schedule() {
    clearTimeout(timer);
    timer = setTimeout(() => { if (document.hidden) schedule(); else refresh(); }, interval);
  }
  async function refresh() {
    if (pending) return;
    pending = true; $("refresh-status").disabled = true;
    const abort = new AbortController(), timeout = setTimeout(() => abort.abort(), 15000);
    try {
      const response = await fetch("/status", { signal: abort.signal, cache: "no-store", credentials: "omit", mode: "same-origin", redirect: "error", headers: { "Accept": "application/json" } });
      if (!response.ok) throw new Error(`Status request failed (HTTP ${response.status}).`);
      render(await response.json());
      text("connection-state", "Live"); $("connection-light").className = "indicator good";
      $("notice").className = "notice hidden";
    } catch (error) {
      text("connection-state", "Unavailable"); $("connection-light").className = "indicator bad";
      verdict("bad", "Status unavailable");
      text("status-summary", "The status service is unavailable. Displayed values are the last received snapshot.");
      text("notice", error.name === "AbortError" ? "Status request timed out. Retrying automatically." : `${error.message} Retrying automatically.`);
      $("notice").className = "notice error";
    } finally { clearTimeout(timeout); pending = false; $("refresh-status").disabled = false; schedule(); }
  }
  $("refresh-status").addEventListener("click", refresh);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });
  refresh();
})();
