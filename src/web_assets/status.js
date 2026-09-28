(() => {
  "use strict";
  const $ = (id) => document.getElementById(id);
  const text = (id, value) => { $(id).textContent = value; };
  const number = (value) => Number.isFinite(Number(value)) ? Number(value) : 0;
  const count = (value) => number(value).toLocaleString();
  let pending = false;
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
  function render(status) {
    const metrics = status.metrics || {}, backends = status.backends || [];
    const checks = status.health_checks_enabled !== false;
    const healthy = checks ? backends.filter((backend) => backend.healthy === true).length : 0;
    const unhealthy = checks ? backends.filter((backend) => backend.healthy === false).length : 0;
    const unknown = backends.length - healthy - unhealthy;
    text("version", status.version ? `v${status.version}` : "");
    text("metric-active", count(metrics.active));
    text("metric-accepted", `${count(metrics.accepted)} accepted since startup`);
    text("metric-backends", unknown === backends.length && backends.length ? "Unmonitored" : `${healthy} / ${backends.length}`);
    text("metric-health", unknown ? `${unknown} without a health result` : "Healthy backends / total");
    text("metric-traffic", bytes(number(metrics.sent) + number(metrics.received)));
    text("metric-directions", `${bytes(metrics.sent)} sent · ${bytes(metrics.received)} received`);
    text("metric-uptime", uptime(status.uptime_seconds));
    text("metric-errors", `${count(metrics.errors)} errors · ${count(metrics.reloads)} reloads`);
    text("status-summary", unhealthy ? `${unhealthy} backend${unhealthy === 1 ? " is" : "s are"} reporting unhealthy. The proxy status service is reachable.` : !checks ? "Proxy is reachable. Backend health checks are disabled." : unknown ? "Proxy is reachable. Some backends do not have a health result." : "Proxy is reachable. All monitored backends are healthy.");
    text("last-update", `Updated ${new Date().toLocaleTimeString()}`);
    $("metrics-link").classList.toggle("hidden", !status.services?.status?.metrics);
    table("listeners", status.listeners || [], 2, (tr, listener) => { cell(tr, listener.name); cell(tr, listener.address, "address"); });
    table("backends", backends, 3, (tr, backend) => {
      cell(tr, backend.name); cell(tr, backend.address, "address");
      const span = document.createElement("span");
      span.className = `health ${!checks ? "unknown" : backend.healthy === false ? "bad" : backend.healthy === true ? "" : "unknown"}`;
      span.textContent = !checks ? "Checks disabled" : backend.healthy === false ? "Unhealthy" : backend.healthy === true ? "Healthy" : "Unknown";
      cell(tr, "").append(span);
    });
    table("metrics", Object.entries(metrics).filter(([, value]) => typeof value === "number"), 2, (tr, [key, value]) => {
      cell(tr, key.replaceAll("_", " ")); cell(tr, count(value), "metric-value");
    });
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
      text("status-summary", "The status service is unavailable. Displayed values are the last received snapshot.");
      text("notice", error.name === "AbortError" ? "Status request timed out. Retrying automatically." : `${error.message} Retrying automatically.`);
      $("notice").className = "notice error";
    } finally { clearTimeout(timeout); pending = false; $("refresh-status").disabled = false; }
  }
  $("refresh-status").addEventListener("click", refresh);
  setInterval(() => { if (!document.hidden) refresh(); }, 5000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) refresh(); });
  refresh();
})();
