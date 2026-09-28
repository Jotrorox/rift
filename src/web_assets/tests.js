// Run with `node src/web_assets/tests.js`. Uses only built-in JavaScript/Node APIs.
// A small DOM and HTTP harness exercises async editor behavior without a browser dependency.
async function testWebAssets(appSource, statusSource) {
  const passed = [];
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  class Element {
    constructor() {
      this.value = ""; this.textContent = ""; this.className = ""; this.children = [];
      this.listeners = {}; this.disabled = false;
      this.classList = { toggle: (name, force) => {
        const classes = new Set(this.className.split(" ").filter(Boolean));
        if (force) classes.add(name); else classes.delete(name);
        this.className = [...classes].join(" ");
      } };
    }
    set innerHTML(_) { throw new Error("Untrusted data must not use innerHTML"); }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    addEventListener(event, listener) { this.listeners[event] = listener; }
    async emit(event, properties = {}) { await this.listeners[event]?.({ preventDefault() {}, ...properties }); }
  }
  class TestHeaders {
    constructor(values = {}) { this.values = new Map(Object.entries(values).map(([k, v]) => [k.toLowerCase(), v])); }
    set(key, value) { this.values.set(key.toLowerCase(), value); }
    get(key) { return this.values.get(key.toLowerCase()) || null; }
    has(key) { return this.values.has(key.toLowerCase()); }
  }
  class TestURL {
    constructor(path, origin) {
      const absolute = /^(https?:\/\/[^/]+)(.*)$/.exec(path);
      this.origin = absolute ? absolute[1] : origin;
      const local = absolute ? absolute[2] : path;
      this.pathname = local.split(/[?#]/)[0];
      this.search = local.includes("?") ? `?${local.split("?")[1].split("#")[0]}` : "";
      this.hash = local.includes("#") ? `#${local.split("#")[1]}` : "";
    }
  }
  async function flush() { for (let i = 0; i < 40; i += 1) await Promise.resolve(); }
  function fixture(script) {
    const elements = new Map(), intervals = [], calls = [];
    const element = (id) => { if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); };
    const document = { getElementById: element, createElement: () => new Element(), hidden: false, addEventListener() {} };
    const window = { location: { origin: "http://localhost:8080" }, addEventListener() {}, confirm: () => true };
    const model = {
      config: { source: "return { -- original comments\n}\n", revision: "rev-1", writable: true },
      status: { version: "1.0", uptime_seconds: 3661, revision: "rev-1", health_checks_enabled: true,
        listeners: [{ name: "public", address: "0.0.0.0:25565" }],
        backends: [{ name: "<script>alert(1)</script>", address: "127.0.0.1:25566", healthy: true }],
        routes: { public: { "*": "lobby" } }, fallbacks: { lobby: ["backup"] },
        metrics: { active: 2, accepted: 7, sent: 1024, received: 2048, errors: 0, reloads: 1 },
        services: { web: { listen: "127.0.0.1:8080", api: true, ui: true }, status: { metrics: true } }, http_hook: true },
      fail: null, intercept: null,
    };
    const response = (data, status = 200) => ({ status, statusText: status === 200 ? "OK" : "Conflict", ok: status >= 200 && status < 300,
      headers: new TestHeaders({ "content-type": "application/json" }), text: async () => JSON.stringify(data), json: async () => data });
    const fetch = async (path, options) => {
      calls.push({ path, options });
      if (model.intercept) { const intercepted = await model.intercept(path, options); if (intercepted) return intercepted; }
      if (model.fail) return response({ error: "Server rejected request" }, model.fail);
      if (path === "/api/status" || path === "/status") return response({ ...model.status });
      if (path === "/api/config" && options.method !== "PUT") return response({ ...model.config });
      if (path === "/api/config/validate") return response({ valid: true });
      if (path === "/api/config" && options.method === "PUT") {
        const body = JSON.parse(options.body);
        if (body.revision !== model.config.revision) return response({ error: "Revision conflict" }, 409);
        model.config.source = body.source; model.config.revision = "rev-saved"; model.status.revision = "rev-saved";
        return response({ revision: "rev-saved", message: "Saved" });
      }
      if (path === "/api/reload") return response({ revision: model.config.revision, message: "Reloaded" });
      return response({ hello: "world" });
    };
    const evaluate = new Function("document", "window", "fetch", "Headers", "AbortController", "TextEncoder", "URL", "setTimeout", "clearTimeout", "setInterval", script);
    evaluate(document, window, fetch, TestHeaders, class { abort() {} },
      class { encode(value) { return { length: unescape(encodeURIComponent(value)).length }; } },
      typeof URL === "undefined" ? TestURL : URL, () => 0, () => {}, (callback) => intervals.push(callback));
    return { element, model, calls, window, intervals, response };
  }
  async function test(name, callback) { await callback(); passed.push(name); }

  await test("initial source, routing, metrics, and safe backend text render", async () => {
    const h = fixture(appSource); await flush();
    assert(h.element("source").value.includes("original comments"), "Source was not loaded intact");
    assert(h.element("metric-active").textContent === "2", "Active connections did not render");
    assert(h.element("backends").children[0].children[0].textContent === "<script>alert(1)</script>", "Backend text was not preserved safely");
    assert(h.element("routes").children[0].children[3].textContent === "backup", "Fallback order did not render");
    assert(h.element("save").disabled, "Pristine source must not be saved");
  });
  await test("external revisions preserve dirty source and block stale saves", async () => {
    const h = fixture(appSource); await flush();
    h.element("source").value = "my changes"; await h.element("source").emit("input");
    h.model.status.revision = "rev-external"; await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("source").value === "my changes", "External revision overwrote edits");
    assert(!h.element("conflict").className.includes("hidden"), "Missing revision warning");
    assert(h.element("save").disabled, "Stale source save must be blocked");
  });
  await test("clean editor synchronizes external source changes", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.revision = "rev-external"; h.model.config.revision = "rev-external"; h.model.config.source = "external source";
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("source").value === "external source", "Clean editor did not synchronize");
  });
  await test("canceling a source refresh preserves unsaved work", async () => {
    const h = fixture(appSource); await flush();
    h.element("source").value = "my changes"; await h.element("source").emit("input"); h.window.confirm = () => false;
    await h.element("refresh-source").emit("click"); await flush();
    assert(h.element("source").value === "my changes", "Canceling discarded changes");
  });
  await test("typing during a source request cannot be overwritten", async () => {
    const h = fixture(appSource); await flush();
    let release;
    h.model.intercept = (path) => path === "/api/config" ? new Promise((resolve) => { release = resolve; }) : undefined;
    const loading = h.element("refresh-source").emit("click"); await flush();
    h.element("source").value = "typed while loading"; await h.element("source").emit("input");
    release(h.response({ source: "server source", revision: "rev-2", writable: true })); await loading; await flush();
    assert(h.element("source").value === "typed while loading", "In-flight load overwrote typing");
  });
  await test("validate never writes source", async () => {
    const h = fixture(appSource); await flush();
    h.element("source").value = "edited"; await h.element("source").emit("input");
    await h.element("validate").emit("click"); await flush();
    assert(h.calls.some(({ path }) => path === "/api/config/validate"), "Validation request missing");
    assert(!h.calls.some(({ options }) => options.method === "PUT"), "Validation wrote source");
  });
  await test("save validates first and sends the original revision", async () => {
    const h = fixture(appSource); await flush();
    h.element("source").value = "edited"; await h.element("source").emit("input");
    await h.element("save").emit("click"); await flush();
    const put = h.calls.find(({ options }) => options.method === "PUT");
    assert(JSON.parse(put.options.body).revision === "rev-1", "Missing optimistic revision");
    assert(h.calls.findIndex(({ path }) => path === "/api/config/validate") < h.calls.indexOf(put), "Save did not validate first");
    assert(h.element("edit-state").textContent === "In sync", "Saved editor remains dirty");
  });
  await test("typing while a save is pending remains unsaved", async () => {
    const h = fixture(appSource); await flush(); let release;
    h.element("source").value = "first edit"; await h.element("source").emit("input");
    h.model.intercept = (path, options) => options.method === "PUT" ? new Promise((resolve) => { release = resolve; }) : undefined;
    const saving = h.element("save").emit("click"); await flush();
    h.element("source").value = "newer edit"; await h.element("source").emit("input");
    h.model.status.revision = "rev-saved"; release(h.response({ revision: "rev-saved", message: "Saved" })); await saving; await flush();
    assert(h.element("source").value === "newer edit", "Save overwrote newer edit");
    assert(h.element("edit-state").textContent === "Unsaved changes", "Newer edit marked saved");
  });
  await test("409 responses preserve source and show the conflict", async () => {
    const h = fixture(appSource); await flush();
    h.element("source").value = "edited"; await h.element("source").emit("input"); h.model.config.revision = "changed-on-disk";
    await h.element("save").emit("click"); await flush();
    assert(h.element("source").value === "edited", "409 discarded source");
    assert(!h.element("conflict").className.includes("hidden"), "409 did not show conflict");
    await h.element("refresh-status").emit("click"); await flush();
    assert(!h.element("conflict").className.includes("hidden"), "Status polling cleared an unresolved disk conflict");
  });
  await test("token uses authorization headers and is removed from its input", async () => {
    const h = fixture(appSource); await flush();
    h.element("token").value = "secret-token-for-test"; await h.element("auth-form").emit("submit"); await flush();
    assert(h.element("token").value === "", "Token input was not cleared");
    assert(h.calls.some(({ options }) => options.headers.get("authorization") === "Bearer secret-token-for-test"), "Authorization header missing");
    assert(!h.calls.some(({ path }) => path.includes("secret-token")), "Token appeared in URL");
  });
  await test("extension console refuses remote URLs", async () => {
    const h = fixture(appSource); await flush();
    h.element("extension-path").value = "https://example.com/ext/private"; h.element("extension-method").value = "GET";
    const before = h.calls.length; await h.element("extension-form").emit("submit"); await flush();
    assert(h.calls.length === before, "Extension request escaped this origin");
  });
  await test("disabled API explains the unavailable service", async () => {
    const h = fixture(appSource); await flush(); h.model.fail = 404;
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("notice").textContent.includes("web.api"), "Disabled API guidance missing");
  });
  await test("oversized source is rejected before HTTP validation", async () => {
    const h = fixture(appSource); await flush();
    h.element("source").value = "x".repeat(256 * 1024 + 1); await h.element("source").emit("input");
    await h.element("validate").emit("click"); await flush();
    assert(!h.calls.some(({ path }) => path === "/api/config/validate"), "Oversized source reached API");
    assert(h.element("config-result").textContent.includes("256 KiB"), "Size guidance missing");
  });
  await test("public status reports disabled checks without claiming health", async () => {
    const h = fixture(statusSource); await flush(); h.model.status.health_checks_enabled = false;
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("metric-backends").textContent === "Unmonitored", "Unmonitored backend reported healthy");
    assert(h.element("status-summary").textContent.includes("disabled"), "Health check status missing");
  });
  await test("public status retains last snapshot when requests fail", async () => {
    const h = fixture(statusSource); await flush(); h.model.fail = 503;
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("metric-active").textContent === "2", "Error removed last snapshot");
    assert(h.element("connection-state").textContent === "Unavailable", "Failed status presented as live");
  });
  return passed;
}

if (typeof module !== "undefined" && require.main === module) {
  const fs = require("node:fs"), path = require("node:path");
  testWebAssets(fs.readFileSync(path.join(__dirname, "app.js"), "utf8"), fs.readFileSync(path.join(__dirname, "status.js"), "utf8"))
    .then((tests) => { for (const test of tests) console.log(`PASS ${test}`); console.log(`${tests.length} web asset tests passed`); })
    .catch((error) => { console.error(error); process.exitCode = 1; });
}
