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
    setAttribute(name, value) { this[name] = value; }
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
    const elements = new Map(), intervals = [], calls = [], timers = new Map();
    let timerId = 0, now = 0;
    const schedule = (callback, delay) => { const id = ++timerId; timers.set(id, { callback, at: now + delay }); return id; };
    const advance = async (delay) => {
      const end = now + delay;
      for (;;) {
        const next = [...timers].filter(([, timer]) => timer.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        now = next[1].at; timers.delete(next[0]); next[1].callback(); await flush();
      }
      now = end; await flush();
    };
    const element = (id) => { if (!elements.has(id)) elements.set(id, new Element()); return elements.get(id); };
    const document = { getElementById: element, createElement: (tag) => {
      const node = new Element();
      if (tag === "textarea") Object.defineProperty(node, "type", { get: () => "textarea" });
      return node;
    }, hidden: false, addEventListener() {}, title: "",
      documentElement: { style: { properties: {}, setProperty(name, value) { this.properties[name] = value; }, removeProperty(name) { delete this.properties[name]; } } } };
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
      access: { name: "local", permissions: null, groups: null },
      definitions: { revision: "rev-1", service_groups: {}, templates: {} },
      deployments: { revision: "rev-1", deployments: [{ id: 1, revision: "rev-1", actor: "local", action: "startup", timestamp_unix_ms: 1 }] },
    };
    const response = (data, status = 200) => ({ status, statusText: status === 200 ? "OK" : "Conflict", ok: status >= 200 && status < 300,
      headers: new TestHeaders({ "content-type": "application/json" }), text: async () => JSON.stringify(data), json: async () => data });
    const fetch = async (path, options) => {
      calls.push({ path, options });
      if (model.intercept) { const intercepted = await model.intercept(path, options); if (intercepted) return intercepted; }
      if (model.fail) return response({ error: "Server rejected request" }, model.fail);
      if (path === "/api/access") return response(model.access);
      if (path === "/api/definitions") return response(model.definitions);
      if (path === "/api/deployments") return response(model.deployments);
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
      typeof URL === "undefined" ? TestURL : URL, schedule, (id) => timers.delete(id), (callback) => intervals.push(callback));
    return { element, model, calls, window, document, intervals, response, advance, TestHeaders };
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
  await test("managed controls encode names, post JSON, and report accepted operations", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [{ name: "lobby west", state: "stopped", players: 0, reservations: 0, automatic_start: true, last_error: null }];
    await h.element("refresh-status").emit("click"); await flush();
    const row = h.element("managed-servers").children[0];
    assert(row.children[0].textContent === "lobby west", "Managed server name missing");
    const [start, stop] = row.children[9].children;
    assert(!start.disabled && !stop.disabled, "Idle managed server controls unavailable");
    await start.emit("click"); await flush();
    const call = h.calls.find(({ path }) => path === "/api/servers/lobby%20west/start");
    assert(call && call.options.method === "POST" && call.options.body === "{}", "Lifecycle operation missing or incorrectly encoded");
    assert(h.element("server-result").textContent.includes("accepted"), "Accepted operation presented as complete");
  });
  await test("servers show their node and the nodes panel appears only with nodes", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [
      { name: "lobby-1", state: "running", players: 0, reservations: 0, automatic_start: true, node: "local" },
      { name: "bedwars-3", state: "unknown", players: 0, reservations: 0, automatic_start: true, node: "node1" },
    ];
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("nodes-panel").hidden, "Nodes panel shown without nodes");
    const rows = h.element("managed-servers").children;
    assert(rows[0].children[7].textContent === "local" && rows[1].children[7].textContent === "node1", "Node column missing");
    assert(rows[1].children[1].className.includes("unknown"), "Unknown state not styled");
    h.model.status.nodes = [
      { name: "node1", state: "unreachable", address: "10.0.0.11", labels: ["games"], instances: 1, max_instances: 40, memory_mb: 32768, allocated_memory_mb: 2048 },
      { name: "node2", state: "online", address: "10.0.0.12", labels: [], instances: 0, max_instances: 128, memory_mb: null, allocated_memory_mb: 0 },
    ];
    await h.element("refresh-status").emit("click"); await flush();
    assert(!h.element("nodes-panel").hidden, "Nodes panel hidden with nodes");
    assert(h.element("node-count").textContent === "1 of 2 online", "Online node count wrong");
    const nodes = h.element("nodes").children;
    assert(nodes[0].children[0].textContent === "node1" && nodes[0].children[3].textContent === "games", "Node row missing");
    assert(nodes[0].children[5].textContent.includes("2,048") || nodes[0].children[5].textContent.includes("2048"), `Node memory missing: ${nodes[0].children[5].textContent}`);
    assert(nodes[1].children[5].textContent.includes("no limit"), "Unlimited memory not explained");
  });
  await test("managed servers cannot be stopped from dashboard while occupied", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [{ name: "lobby", state: "running", players: 1, reservations: 0, automatic_start: true }];
    await h.element("refresh-status").emit("click"); await flush();
    const [start, stop] = h.element("managed-servers").children[0].children[9].children;
    assert(start.disabled && stop.disabled, "Occupied running server controls must be disabled");
  });
  await test("lifecycle conflicts do not mark the source editor stale", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [{ name: "lobby", state: "running", players: 0, reservations: 0, automatic_start: true }];
    await h.element("refresh-status").emit("click"); await flush();
    h.model.intercept = (path) => path === "/api/servers/lobby/stop" ? h.response({ error: "Backend became occupied" }, 409) : undefined;
    await h.element("managed-servers").children[0].children[9].children[1].emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("occupied"), "Lifecycle error was not displayed");
    assert(h.element("conflict").className.includes("hidden"), "Lifecycle conflict incorrectly affected configuration revision");
  });
  await test("groups safely render template and storage and create instances with encoded names", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.service_groups = [{ name: "arena west", port_range: [25600, 25610], storage: "disposable", template: "<script>arena</script>", instances: [] }];
    h.model.intercept = (path) => path === "/api/groups/arena%20west/instances" ? h.response({ name: "arena-1", storage: "disposable" }, 201) : undefined;
    await h.element("refresh-status").emit("click"); await flush();
    const row = h.element("service-groups").children[0];
    assert(row.children[2].textContent === "Disposable game", "Disposable group policy missing");
    assert(row.children[3].textContent === "<script>arena</script>", "Template text was not preserved safely");
    assert(row.children[1].textContent === "25600–25610", "Port range missing");
    await row.children[5].children[0].emit("click"); await flush();
    const call = h.calls.find(({ path }) => path === "/api/groups/arena%20west/instances");
    assert(call && call.options.method === "POST" && call.options.body === "{}", "Instance creation request incorrect");
    assert(h.element("server-result").textContent.includes("arena-1"), "Created instance name missing");
    assert(h.element("server-result").textContent.includes("EULA"), "Provisioning prerequisite missing");
  });
  await test("wake status explains paused and exhausted recovery and full groups block creation", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [
      { name: "paused", state: "stopped", automatic_start: false, automatic_enabled: false },
      { name: "exhausted", state: "failed", automatic_start: true, automatic_enabled: true, restart_exhausted: true },
      { name: "manual", state: "stopped", automatic_start: false, automatic_enabled: true },
      { name: "auto", state: "stopped", automatic_start: true, automatic_enabled: true },
    ];
    h.model.status.service_groups = [
      { name: "full", storage: "persistent", instances: ["full-1"], scaling: { max_instances: 1 } },
      { name: "open", storage: "persistent", instances: ["open-1"], scaling: { max_instances: 2 } },
    ];
    await h.element("refresh-status").emit("click"); await flush();
    const wake = h.element("managed-servers").children.map((row) => row.children[3].textContent);
    assert(wake[0].includes("Paused by Stop") && wake[1].includes("retries exhausted"), "Paused wake reasons missing");
    assert(wake[2] === "Explicit start only" && wake[3] === "Enabled", "Wake policy labels incorrect");
    const [full, open] = h.element("service-groups").children.map((row) => row.children[5].children[0]);
    assert(full.disabled && full.title.includes("max_instances"), "Full group creation was enabled");
    assert(!open.disabled, "Group below its maximum was blocked");
  });
  await test("instance removal distinguishes destructive games and persistent worlds", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [
      { name: "game west", group: "arena", address: "127.0.0.1:25600", template: "arena", storage: "disposable", state: "stopped" },
      { name: "survival-1", group: "survival", storage: "persistent", state: "stopped" },
      { name: "static", group: null, storage: "persistent", state: "stopped" },
    ];
    h.model.intercept = (path) => path.startsWith("/api/instances/") ? h.response({ removed: true, files_removed: path.includes("game") }) : undefined;
    await h.element("refresh-status").emit("click"); await flush();
    const rows = h.element("managed-servers").children;
    assert(rows[0].children[4].textContent === "Disposable game", "Disposable policy missing");
    assert(rows[1].children[4].textContent === "Persistent world", "Persistent policy missing");
    assert(rows[0].children[5].textContent === "arena", "Instance template missing");
    assert(rows[0].children[9].children[2].textContent === "Remove & delete files", "Destructive removal label missing");
    assert(rows[1].children[9].children[2].textContent === "Remove · keep files", "Persistent removal label missing");
    assert(rows[2].children[9].children.length === 2, "Static managed servers must not expose dynamic removal");
    await rows[0].children[9].children[2].emit("click"); await flush();
    const call = h.calls.find(({ path }) => path === "/api/instances/game%20west");
    assert(call && call.options.method === "DELETE" && call.options.body === "{}", "Instance removal request incorrect");
    assert(h.element("server-result").textContent.includes("deleted"), "Disposable cleanup outcome missing");
    await h.element("managed-servers").children[1].children[9].children[2].emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("remain on disk"), "Persistent retention outcome missing");
  });
  await test("dashboard polls a removal beyond 15 seconds and prevents duplicate submissions", async () => {
    const h = fixture(appSource); await flush(); let complete = false;
    h.model.status.managed_servers = [{ name: "arena-1", group: "arena", storage: "disposable", state: "running" }];
    h.model.intercept = (path) => {
      if (path === "/api/instances/arena-1") return h.response({ operation_id: "slow-removal", status: "pending" }, 202);
      if (path === "/api/operations/slow-removal") return h.response(complete
        ? { status: "succeeded", http_status: 200, result: { removed: true, files_removed: true } }
        : { status: "pending" });
    };
    await h.element("refresh-status").emit("click"); await flush();
    const remove = h.element("managed-servers").children[0].children[9].children[2];
    await remove.emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("slow-removal"), "Pending operation ID missing");
    await h.advance(16000);
    assert(remove.disabled, "Removal controls were enabled before completion");
    assert(!h.element("server-result").textContent.includes("timed out"), "Long operation hit the dashboard request timeout");
    assert(h.calls.filter(({ path }) => path === "/api/operations/slow-removal").length >= 16, "Dashboard stopped polling");
    await remove.emit("click"); await flush();
    assert(h.calls.filter(({ options }) => options.method === "DELETE").length === 1, "Pending removal was duplicated");
    complete = true; await h.advance(1000);
    assert(h.element("server-result").textContent.includes("deleted"), "Completed removal outcome missing");
    assert(!h.element("managed-servers").children[0].children[9].children[2].disabled, "Completion did not restore controls");
  });
  await test("asynchronous failures show their operation ID and preserve the configuration editor", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.service_groups = [{ name: "arena", storage: "disposable" }];
    h.model.intercept = (path) => {
      if (path === "/api/groups/arena/instances") return h.response({ operation_id: "failed-create" }, 202);
      if (path === "/api/operations/failed-create") return h.response({ status: "failed", http_status: 409, error: "No free ports" });
    };
    await h.element("refresh-status").emit("click"); await flush();
    await h.element("service-groups").children[0].children[5].children[0].emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("failed-create failed: No free ports"), "Operation failure missing");
    assert(h.element("server-result").className.includes("error"), "Failure presented as success");
    assert(h.element("conflict").className.includes("hidden"), "Operation failure changed editor revision state");
    assert(!h.element("service-groups").children[0].children[5].children[0].disabled, "Failure did not restore controls");
  });
  await test("polling denial or expiry explains the unresolved operation without claiming failure", async () => {
    for (const status of [403, 404, 503]) {
      const h = fixture(appSource); await flush();
      h.model.status.service_groups = [{ name: "arena", storage: "disposable" }];
      h.model.intercept = (path) => {
        if (path === "/api/groups/arena/instances") return h.response({ operation_id: "unresolved" }, 202);
        if (path === "/api/operations/unresolved") return h.response({ error: "Result unavailable" }, status);
      };
      await h.element("refresh-status").emit("click"); await flush();
      await h.element("service-groups").children[0].children[5].children[0].emit("click"); await flush();
      const message = h.element("server-result").textContent;
      assert(message.includes("Result unavailable") && message.includes("/api/operations/unresolved") && message.includes("may still be running"), "Unresolved operation guidance missing");
      if (status === 403) assert(h.element("auth-panel").open, "Polling denial did not expose token controls");
    }
  });
  await test("asynchronous cleanup errors remain warnings", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [{ name: "arena-1", group: "arena", storage: "disposable", state: "stopped" }];
    h.model.intercept = (path) => {
      if (path === "/api/instances/arena-1") return h.response({ operation_id: "cleanup" }, 202);
      if (path === "/api/operations/cleanup") return h.response({ status: "succeeded", result: { removed: true, files_removed: false, cleanup_error: "Directory is read-only" } });
    };
    await h.element("refresh-status").emit("click"); await flush();
    await h.element("managed-servers").children[0].children[9].children[2].emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("read-only"), "Asynchronous cleanup error missing");
    assert(h.element("server-result").className.includes("warning"), "Asynchronous cleanup error presented as success");
  });
  await test("occupied, reserved and removing instances disable removal", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [
      { name: "occupied", group: "games", state: "running", players: 1 },
      { name: "reserved", group: "games", state: "running", reservations: 1 },
      { name: "removing", group: "games", state: "removing" },
      { name: "stopping", group: "games", state: "stopping" },
    ];
    await h.element("refresh-status").emit("click"); await flush();
    for (const row of h.element("managed-servers").children) {
      const remove = row.children[9].children[2];
      assert(remove.disabled, "Unsafe instance removal enabled");
      await remove.emit("click"); await flush();
    }
    assert(!h.calls.some(({ options }) => options.method === "DELETE"), "Disabled removal submitted a request");
  });
  await test("provisioning busy state prevents duplicate requests and restores controls", async () => {
    const h = fixture(appSource); await flush(); let release;
    h.model.status.service_groups = [{ name: "arena", port_range: [25600, 25610], storage: "disposable", template: "arena" }];
    h.model.intercept = (path) => path === "/api/groups/arena/instances" ? new Promise((resolve) => { release = resolve; }) : undefined;
    await h.element("refresh-status").emit("click"); await flush();
    const create = h.element("service-groups").children[0].children[5].children[0];
    await create.emit("click"); await flush();
    assert(create.disabled, "Pending creation control remains enabled");
    await create.emit("click"); await flush();
    assert(h.calls.filter(({ path }) => path === "/api/groups/arena/instances").length === 1, "Duplicate provisioning request");
    release(h.response({ name: "arena-1" }, 201)); await flush();
    assert(!h.element("service-groups").children[0].children[5].children[0].disabled, "Creation controls were not restored");
  });
  await test("removal cleanup failures warn without claiming files deleted or changing editor revision", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.managed_servers = [{ name: "arena-1", group: "arena", storage: "disposable", state: "stopped" }];
    h.model.intercept = (path) => path === "/api/instances/arena-1" ? h.response({ removed: true, files_removed: false, cleanup_error: "Directory is read-only" }) : undefined;
    await h.element("refresh-status").emit("click"); await flush();
    await h.element("managed-servers").children[0].children[9].children[2].emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("read-only"), "Cleanup error missing");
    assert(h.element("server-result").className.includes("warning"), "Incomplete cleanup presented as success");
    assert(!h.element("server-result").textContent.includes("deleted"), "Failed cleanup claimed deletion");
    h.model.intercept = (path) => path === "/api/instances/arena-1" ? h.response({ error: "Instance became occupied" }, 409) : undefined;
    await h.element("managed-servers").children[0].children[9].children[2].emit("click"); await flush();
    assert(h.element("server-result").textContent.includes("occupied"), "Removal conflict missing");
    assert(h.element("conflict").className.includes("hidden"), "Removal conflict changed editor revision state");
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
    assert(h.element("metric-connections").textContent.startsWith("2 active connections"), "Error removed last snapshot");
    assert(h.element("connection-state").textContent === "Unavailable", "Failed status presented as live");
  });
  await test("public exposure shows display names and players without operator detail", async () => {
    const h = fixture(statusSource); await flush();
    h.model.status = { exposure: "public", uptime_seconds: 90, players: 12, health_checks_enabled: true,
      backends: [{ name: "survival1", display: "<b>Survival</b>", healthy: true, players: 9 }, { name: "lobby", display: "Lobby", healthy: false, players: 3 }],
      services: { status: { metrics: false } }, page: { panels: { summary: true, backends: true, listeners: false, metrics: false } } };
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("metric-players").textContent === "12", "Network players missing");
    const row = h.element("backends").children[0];
    assert(row.children.length === 3, "Public rows must not include an address column");
    assert(row.children[0].textContent === "<b>Survival</b>", "Display name was not rendered as text");
    assert(row.children[1].textContent === "9", "Backend players missing");
    assert(row.children[2].children[0].textContent === "Online", "Public health wording missing");
    assert(h.element("backend-address-heading").className.includes("hidden"), "Address heading shown publicly");
    assert(h.element("listeners-panel").className.includes("hidden"), "Listeners shown publicly");
    assert(h.element("metrics-panel").className.includes("hidden"), "Counters shown publicly");
    assert(h.element("traffic-stat").className.includes("hidden"), "Traffic shown publicly");
    assert(h.element("status-verdict").textContent === "1 of 2 servers offline", "Public verdict wording missing");
    assert(h.element("version").textContent === "", "Version shown without one");
  });
  await test("status page settings brand, theme, link and pace the page", async () => {
    const h = fixture(statusSource); await flush();
    h.model.status.page = { title: "Example Network", notice: "Maintenance at 02:00", accent: "#ff6a00", refresh_ms: 12000,
      panels: { summary: false, backends: true, listeners: true, metrics: true },
      links: [{ label: "Discord", href: "https://discord.example/invite" }, { label: "Bad", href: "javascript:alert(1)" }, { label: "Rules", href: "/rules" }, { label: "Escape", href: "//evil.example" }] };
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("brand-name").textContent === "Example Network", "Title not applied");
    assert(h.document.title === "Network status · Example Network", "Document title not applied");
    assert(h.document.documentElement.style.properties["--accent"] === "#ff6a00", "Accent not applied");
    assert(h.document.documentElement.style.properties["--accent-soft"] === "#ff6a001f", "Soft accent not derived");
    assert(h.element("announcement").textContent === "Maintenance at 02:00" && !h.element("announcement").className.includes("hidden"), "Notice not shown");
    assert(h.element("summary").className.includes("hidden"), "Hidden summary still shown");
    const links = h.element("links").children;
    assert(links.length === 2 && links[0].href === "https://discord.example/invite" && links[1].href === "/rules", "Unsafe links were not filtered");
    assert(links[0].rel === "noopener noreferrer", "External link opener not isolated");
    assert(h.element("refresh-interval").textContent === "Refreshes every 12 seconds", "Refresh interval not shown");
    const before = h.calls.filter(({ path }) => path === "/status").length;
    await h.advance(11000);
    assert(h.calls.filter(({ path }) => path === "/status").length === before, "Refreshed before the configured interval");
    await h.advance(1000);
    assert(h.calls.filter(({ path }) => path === "/status").length === before + 1, "Did not refresh at the configured interval");
    h.model.status.page = { refresh_ms: 10 };
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("refresh-interval").textContent === "Refreshes every second", "Refresh interval was not clamped");
    assert(h.document.documentElement.style.properties["--accent"] === undefined, "Removed accent was retained");
    assert(h.element("brand-name").textContent === "Rift" && h.element("announcement").className.includes("hidden"), "Removed settings were retained");
  });
  await test("scoped operators hide configuration and disable ungranted mutations", async () => {
    const h = fixture(appSource); await flush();
    h.model.access = { name: "lobby_staff", permissions: ["read", "logs"], groups: ["lobby"] };
    h.model.status.managed_servers = [{ name: "lobby-1", state: "stopped", players: 0, reservations: 0 }];
    const before = h.calls.length;
    h.element("token").value = "staff-token"; await h.element("auth-form").emit("submit"); await flush();
    assert(h.element("config-panel").className.includes("hidden"), "Privileged editor was exposed");
    assert(h.element("source").value === "", "Previous operator source was retained");
    assert(!h.calls.slice(before).some(({ path }) => path === "/api/config" || path === "/api/definitions"), "Scoped operator fetched privileged data");
    assert(h.element("managed-servers").children[0].children[9].children[0].disabled, "Ungrantable server mutation was enabled");
  });
  await test("plugins render safely, restart with an encoded name and tail their log", async () => {
    const h = fixture(appSource); await flush();
    const plugins = [
      { name: "(script)", mode: "script", engine: "lua", status: "active", restarts: 0, calls: 0, restartable: false },
      { name: "hello lua", mode: "persistent", engine: "lua", version: "0.1.0", status: "failed", restarts: 3, calls: 1200, last_error: "<b>init: boom</b>", restartable: true },
      { name: "(remote)", mode: "remote", engine: null, status: "active", restarts: 0, calls: 0, restartable: false },
    ];
    h.model.intercept = (path, options) => {
      if (path === "/api/plugins") return h.response({ plugins });
      if (path === "/api/plugins/hello%20lua/logs") return h.response({ plugin: "hello lua", lines: [{ timestamp_unix_ms: 1, level: "error", message: "lifecycle: failed: <script>" }] });
      if (path === "/api/plugins/hello%20lua/restart" && options.method === "POST") return h.response({ accepted: true }, 202);
      return undefined;
    };
    h.window.location.hash = "#plugins";
    await h.element("refresh-status").emit("click"); await flush();
    assert(!h.element("view-plugins").hidden, "Plugins view not shown");
    const rows = h.element("plugins").children;
    assert(rows.length === 3, "Every mode should be listed");
    assert(rows[0].children[1].textContent === "Configuration script" && rows[2].children[1].textContent === "Bus responders", "Modes not labelled");
    assert(rows[1].children[0].textContent === "hello lua 0.1.0", "Name and version missing");
    assert(rows[1].children[3].className.includes("failed"), "Failed state not styled");
    assert(rows[1].children[6].textContent === "<b>init: boom</b>", "Last error not preserved as text");
    assert(rows[0].children[7].children.length === 0, "The configuration script cannot be restarted");
    const restart = rows[1].children[7].children[0];
    assert(!restart.disabled, "Restart unavailable to an unrestricted operator");
    const select = h.element("plugin-log-select");
    assert(select.children.length === 2 && select.children[1].value === "hello lua", "Only persistent plugins have a log");
    select.value = "hello lua"; await select.emit("change"); await flush();
    assert(h.element("plugin-log-output").textContent.includes("lifecycle: failed: <script>"), "Log tail missing");
    await restart.emit("click"); await flush();
    const call = h.calls.find(({ path }) => path === "/api/plugins/hello%20lua/restart");
    assert(call && call.options.method === "POST" && call.options.body === "{}", "Restart request missing or incorrectly encoded");
    assert(h.element("plugin-result").textContent.includes("accepted"), "Accepted restart not reported");
  });
  await test("restarts need the plugins grant and scoped operators see no plugins", async () => {
    const h = fixture(appSource); await flush();
    h.model.intercept = (path) => path === "/api/plugins" ? h.response({ plugins: [{ name: "a", mode: "persistent", engine: "wasm", status: "running", restartable: true }] }) : undefined;
    h.model.access = { name: "viewer", permissions: ["read"], groups: null };
    h.element("token").value = "viewer-token"; await h.element("auth-form").emit("submit"); await flush();
    h.window.location.hash = "#plugins";
    await h.element("refresh-status").emit("click"); await flush();
    assert(h.element("plugins").children[0].children[7].children[0].disabled, "Restart enabled without the plugins grant");
    assert(h.element("plugin-log-panel").className.includes("hidden"), "Log tail shown without the logs grant");
    const scoped = fixture(appSource); await flush();
    scoped.model.access = { name: "lobby_staff", permissions: ["read", "logs"], groups: ["lobby"] };
    scoped.element("token").value = "staff-token"; await scoped.element("auth-form").emit("submit"); await flush();
    assert(scoped.element("nav-plugins").className.includes("hidden"), "Plugins shown to a group-scoped operator");
    assert(!scoped.calls.some(({ path }) => path.startsWith("/api/plugins")), "Scoped operator fetched plugins");
  });
  await test("server logs resume their cursor and discard responses for a previous selection", async () => {
    const h = fixture(appSource); await flush();
    h.model.intercept = (path) => path.includes("/logs") ? h.response({ text: "first\n", cursor: 6 }) : null;
    h.element("console-server").value = "lobby west"; await h.element("console-server").emit("change"); await flush();
    assert(h.element("console-output").textContent === "first\n", "Log output did not render");
    await h.element("console-refresh").emit("click"); await flush();
    assert(h.calls.some(({ path }) => path === "/api/servers/lobby%20west/logs?cursor=6"), "Log cursor or encoded name missing");
    let resolve;
    h.model.intercept = () => new Promise((done) => { resolve = done; });
    const pending = h.element("console-refresh").emit("click"); await flush();
    h.element("console-server").value = "other"; await h.element("console-server").emit("change");
    resolve(h.response({ text: "wrong server", cursor: 99 })); await pending; await flush();
    assert(!h.element("console-output").textContent.includes("wrong server"), "Old server output leaked into the selected console");
  });
  await test("structured fields send typed definitions with the loaded revision", async () => {
    const h = fixture(appSource); await flush();
    h.element("definition-section").value = "templates"; await h.element("definition-section").emit("change"); await flush();
    h.element("definition-name").value = "arena";
    const inputs = h.element("definition-fields").children.map((wrapper) => wrapper.children[1]);
    inputs.find((input) => input.id === "definition-field-server_jar").value = "assets/paper.jar";
    inputs.find((input) => input.id === "definition-field-plugins").value = "assets/a.jar\nassets/b.jar";
    let submitted;
    h.model.intercept = (path, options) => {
      if (options.method === "PUT" && path.includes("/definitions/")) { submitted = JSON.parse(options.body); return h.response({ revision: "rev-1" }); } return null;
    };
    await h.element("definition-form").emit("submit"); await flush();
    assert(submitted.revision === "rev-1", "Definition revision check was omitted");
    assert(submitted.definition.server_jar === "assets/paper.jar" && submitted.definition.plugins.length === 2, "Fields were not serialized as a structured definition");
  });
  await test("software templates serialize a nested software table and suggest versions", async () => {
    const h = fixture(appSource); await flush();
    h.element("definition-section").value = "templates"; await h.element("definition-section").emit("change"); await flush();
    h.element("definition-name").value = "lobby";
    // Saving re-renders the form, so look fields up each time.
    const field = (key) => h.element("definition-fields").children.map((wrapper) => wrapper.children[1]).find((input) => input.id === `definition-field-${key}`);
    h.model.intercept = (path, options) => {
      if (path === "/api/software/paper/versions") return h.response({ type: "paper", versions: ["1.21.8", "1.21.7"] });
      if (options.method === "PUT" && path.includes("/definitions/")) { h.submitted = JSON.parse(options.body); return h.response({ revision: "rev-1" }); }
      return null;
    };
    field("software-type").value = "paper"; await field("software-type").emit("change"); await flush();
    assert(field("software-version").list === "software-versions", "Version input lacks suggestions");
    assert(h.element("software-versions").children.map((option) => option.value).join() === "latest,1.21.8,1.21.7", "Versions were not suggested");
    field("software-version").value = "1.21.8";
    field("software-url").value = "https://ignored.example/server.jar";
    field("configs").value = "local/templates/Global/server\nlocal/templates/Lobby/default";
    field("accept_eula").value = "true";
    await h.element("definition-form").emit("submit"); await flush();
    const definition = h.submitted.definition;
    assert(definition.software.type === "paper" && definition.software.version === "1.21.8", "Software was not nested");
    assert(definition.software.url === "https://ignored.example/server.jar" && definition.server_jar === undefined, "Unexpected jar fields");
    assert(definition.configs.length === 2 && definition.accept_eula === true, "Template folders or EULA missing");
    h.element("definition-name").value = "legacy"; field("software-type").value = ""; field("server_jar").value = "assets/paper.jar";
    await h.element("definition-form").emit("submit"); await flush();
    assert(h.submitted.definition.software === undefined, "A local jar template still sent software");
  });
  await test("templates show cache state and poll software downloads", async () => {
    const h = fixture(appSource);
    let polls = 0;
    h.model.intercept = (path, options) => {
      if (path === "/api/templates") return h.response({ templates: [
        { name: "lobby", software: { type: "paper", version: "1.21.8", cached: polls > 1, resolved_version: "1.21.8", build: "60", size: 1048576 }, groups: ["lobby"] },
        { name: "legacy", software: null, groups: [] },
      ] });
      if (path === "/api/templates/lobby/download") { h.downloadBody = JSON.parse(options.body); return h.response({ operation_id: "op1", status: "pending", poll: "/api/operations/op1" }, 202); }
      if (path === "/api/operations/op1") { polls += 1; return h.response(polls === 1 ? { status: "pending" } : { status: "succeeded", result: { template: "lobby", software: { type: "paper", version: "1.21.8", resolved_version: "1.21.8" } } }); }
      return null;
    };
    await flush();
    const rows = h.element("templates").children;
    assert(rows[0].children[1].textContent === "paper 1.21.8" && rows[0].children[2].textContent.startsWith("Not downloaded"), "Software state missing");
    assert(rows[1].children[1].textContent === "Local server jar" && rows[1].children[4].children.length === 0, "Local jars must not offer downloads");
    await rows[0].children[4].children[0].emit("click"); await flush();
    assert(h.downloadBody.refresh === false, "First download must not force a refresh");
    await h.advance(1000);
    assert(h.element("template-result").textContent === "Downloaded paper 1.21.8 for lobby.", h.element("template-result").textContent);
    const updated = h.element("templates").children[0];
    assert(updated.children[2].textContent.includes("build 60") && updated.children[4].children[0].textContent === "Update", "Cache state did not refresh");
  });
  await test("file browser encodes paths, edits text files and deletes entries", async () => {
    const h = fixture(appSource); await flush();
    const text = (body, status = 200) => ({ status, statusText: "OK", ok: status < 300, headers: new TestHeaders({ "content-type": "application/octet-stream" }), text: async () => body });
    h.model.intercept = (path, options) => {
      if (path === "/api/files/templates/Lobby/my%20default" && options.method !== "DELETE") return h.response({ path: "Lobby/my default", entries: [{ name: "server.properties", type: "file", size: 12, modified: 1 }, { name: "plugins", type: "directory" }], truncated: false });
      if (path === "/api/files/templates/Lobby/my%20default/server.properties" && !options.method) return text("motd=old\n");
      if (path.startsWith("/api/files/") && options.method === "PUT") { h.written = { path, body: options.body, type: options.headers.get("Content-Type") }; return h.response({ path, type: "file" }); }
      if (path.startsWith("/api/files/") && options.method === "DELETE") { h.deleted = path; return h.response({ deleted: path }); }
      return null;
    };
    h.element("files-path").value = "/Lobby//my default/"; await h.element("files-form").emit("submit"); await flush();
    assert(h.element("files-path").value === "Lobby/my default", "Path was not normalized");
    const [file, folder] = h.element("files-entries").children;
    assert(file.children[0].children[0].textContent === "server.properties" && folder.children[0].children[0].textContent === "plugins/", "Entries did not render");
    await file.children[0].children[0].emit("click"); await flush();
    assert(h.element("files-content").value === "motd=old\n" && h.element("files-save").disabled, "File did not open unchanged");
    h.element("files-content").value = "motd=new\n"; await h.element("files-content").emit("input");
    assert(!h.element("files-save").disabled, "Edited file cannot be saved");
    await h.element("files-save").emit("click"); await flush();
    assert(h.written.path === "/api/files/templates/Lobby/my%20default/server.properties" && h.written.body === "motd=new\n" && h.written.type === "application/octet-stream", "File save was not a raw PUT");
    h.element("files-new-name").value = "worlds"; await h.element("files-new-folder").emit("click"); await flush();
    assert(h.written.path === "/api/files/templates/Lobby/my%20default/worlds?directory", "Folder creation path was wrong");
    await h.element("files-entries").children[1].children[3].children[0].emit("click"); await flush();
    assert(h.deleted === "/api/files/templates/Lobby/my%20default/plugins", "Folder deletion path was wrong");
  });
  await test("rollback uses the displayed history revision and keeps conflicts visible", async () => {
    const h = fixture(appSource); await flush();
    h.model.status.revision = "newer-revision";
    h.model.intercept = (path) => path.endsWith("/rollback") ? h.response({ error: "Deployment revision conflict" }, 409) : null;
    await h.element("deployments").children[0].children[4].children[0].emit("click"); await flush();
    const call = h.calls.find(({ path }) => path.endsWith("/rollback"));
    assert(JSON.parse(call.options.body).revision === "rev-1", "Rollback silently refreshed its expected revision");
    assert(h.element("deployment-result").textContent.includes("conflict"), "Stale rollback conflict was hidden");
  });
  await test("server list edits send components and plain text and keep Lua-only fields out", async () => {
    const h = fixture(appSource);
    h.model.definitions.server_list = { motd: { text: "Old", color: "gold" }, version: null, max_players: 20, sample: ["a"], favicon: "data:image/png;base64,AAAA",
      hosts: { "play.example.com": { motd: [{ text: "<b>Host</b>" }], version: "1.21", max_players: null } } };
    await flush();
    assert(h.element("server-list-enabled").checked, "Configured server list shown as off");
    assert(JSON.parse(h.element("server-list-motd").value).color === "gold", "Component MOTD not rendered as JSON");
    assert(!h.element("server-list-favicon").hidden && h.element("server-list-favicon").src.startsWith("data:image/png"), "Favicon preview missing");
    assert(h.element("server-list-hosts").children[0].children[1].textContent === "<b>Host</b>", "Host MOTD summary unsafe or missing");
    h.element("server-list-motd").value = "{player}'s network\nline two"; h.element("server-list-maintenance-motd").value = '{"text":"Soon","color":"red"}';
    h.element("server-list-max-players").value = "50"; await h.element("server-list-form").emit("input");
    let submitted;
    h.model.intercept = (path, options) => { if (options.method === "PUT") { submitted = { path, body: JSON.parse(options.body) }; return h.response({ revision: "rev-1" }); } return null; };
    await h.element("server-list-form").emit("submit"); await flush();
    assert(submitted.path === "/api/definitions/server_list" && submitted.body.revision === "rev-1", "Server list request missing its revision");
    const value = submitted.body.definition;
    assert(value.motd === "{player}'s network\nline two", "Brace-prefixed plain text was not kept as text");
    assert(value.maintenance_motd.color === "red" && value.max_players === 50 && value.sample[0] === "a", "Fields were not serialized");
    assert(!("favicon" in value) && !("hosts" in value), "Lua-only fields were sent");
    h.element("server-list-max-players").value = "-1"; const before = h.calls.length;
    await h.element("server-list-form").emit("submit"); await flush();
    assert(h.calls.length === before && h.element("server-list-result").textContent.includes("nonnegative"), "Invalid maximum reached the API");
  });
  await test("server icons upload as 64-pixel PNG data URLs, persist until saved and can be removed", async () => {
    const h = fixture(appSource);
    h.model.definitions.server_list = { motd: "Hi", favicon: null, hosts: {} };
    await flush();
    assert(h.element("server-list-favicon").hidden && h.element("server-list-favicon-remove").hidden, "Empty icon shown");
    const png = (width, height, extra = 0) => {
      const bytes = new Uint8Array(33 + extra), view = new DataView(bytes.buffer);
      bytes.set([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82]); view.setUint32(16, width); view.setUint32(20, height);
      return { type: "image/png", size: bytes.length, arrayBuffer: async () => bytes.buffer };
    };
    let submitted;
    h.model.intercept = (path, options) => { if (options.method === "PUT") { submitted = JSON.parse(options.body).definition; return h.response({ revision: "rev-1" }); } return null; };
    // An exact 64×64 PNG is kept byte for byte.
    h.element("server-list-favicon-file").files = [png(64, 64)];
    await h.element("server-list-favicon-file").emit("change"); await flush();
    assert(h.element("server-list-favicon").src.startsWith("data:image/png;base64,iVBORw0KGgo") && !h.element("server-list-favicon").hidden, "Uploaded icon not previewed");
    assert(h.element("server-list-favicon-state").textContent.includes("Save"), "Pending icon not explained");
    h.model.definitions.server_list = { motd: "Elsewhere", favicon: null, hosts: {} }; await h.advance(5000);
    assert(h.element("server-list-favicon-state").textContent.includes("Save"), "Refresh discarded the chosen icon");
    await h.element("server-list-form").emit("submit"); await flush();
    assert(submitted.favicon.startsWith("data:image/png;base64,iVBORw0KGgo") && submitted.motd === "Hi", "Icon not sent with the form");
    // Other images are drawn onto a 64×64 canvas, keeping their aspect ratio.
    let drawn;
    h.window.createImageBitmap = async () => ({ width: 256, height: 128, close() {} });
    const createElement = h.document.createElement;
    h.document.createElement = (tag) => tag !== "canvas" ? createElement(tag) : { getContext: () => ({ drawImage: (...args) => { drawn = args.slice(1); } }), toDataURL: () => "data:image/png;base64,QUJD" };
    h.element("server-list-favicon-file").files = [{ type: "image/jpeg", size: 9, arrayBuffer: async () => new ArrayBuffer(9) }];
    await h.element("server-list-favicon-file").emit("change"); await flush();
    assert(JSON.stringify(drawn) === "[0,16,64,32]", `Image not fitted inside the icon: ${drawn}`);
    assert(h.element("server-list-favicon").src === "data:image/png;base64,QUJD", "Resized icon not previewed");
    // Images the browser cannot read leave the chosen icon untouched.
    h.window.createImageBitmap = async () => { throw new Error("decode"); };
    h.element("server-list-favicon-file").files = [png(32, 32)];
    await h.element("server-list-favicon-file").emit("change"); await flush();
    assert(h.element("server-list-favicon-state").textContent.includes("isn’t an image"), "Unreadable image not reported");
    assert(h.element("server-list-favicon").src === "data:image/png;base64,QUJD", "Failed upload replaced the chosen icon");
    // Removing a configured icon sends false; with none configured, nothing.
    h.model.definitions.server_list = { motd: "Hi", favicon: "data:image/png;base64,AAAA", hosts: {} };
    await h.element("server-list-refresh").emit("click"); await flush();
    assert(h.element("server-list-favicon-state").textContent === "Current icon", "Configured icon not shown");
    await h.element("server-list-favicon-remove").emit("click");
    assert(h.element("server-list-favicon").hidden && h.element("server-list-favicon-state").textContent.includes("removed"), "Removal not previewed");
    await h.element("server-list-form").emit("submit"); await flush();
    assert(submitted.favicon === false, "Removal not sent");
    h.model.definitions.server_list = { motd: "Hi", favicon: null, hosts: {} };
    await h.element("server-list-refresh").emit("click"); await flush();
    await h.element("server-list-favicon-remove").emit("click");
    await h.element("server-list-form").emit("submit"); await flush();
    assert(!("favicon" in submitted), "Removing a missing icon was sent");
  });
  await test("turning the tab list off sends null and network listing is explicit", async () => {
    const h = fixture(appSource);
    h.model.definitions.tab_list = { header: "Hi {player}", footer: null, players: "server" };
    await flush();
    assert(h.element("tab-list-header").value === "Hi {player}", "Header not rendered");
    const bodies = [];
    h.model.intercept = (path, options) => { if (options.method === "PUT") { bodies.push(JSON.parse(options.body).definition); return h.response({ revision: "rev-1" }); } return null; };
    h.element("tab-list-players").value = "network"; await h.element("tab-list-form").emit("submit"); await flush();
    assert(!h.element("tab-list-header").disabled, "Enabled tab list fields were disabled");
    h.element("tab-list-enabled").checked = false; await h.element("tab-list-form").emit("change");
    assert(h.element("tab-list-header").disabled && h.element("tab-list-players").disabled, "Fields stayed editable for a disabled tab list");
    await h.element("tab-list-form").emit("submit"); await flush();
    assert(bodies[0].header === "Hi {player}" && bodies[0].players === "network" && !("footer" in bodies[0]), "Tab list fields incorrect");
    assert(bodies[1] === null, "Disabled tab list was not removed");
  });
  const PLAYER = "0123456789abcdef0123456789abcdef";
  // A small /api/perms model; `calls` records every write with its parsed body.
  function permsModel(h, overrides = {}) {
    const calls = [];
    const data = {
      groups: [
        { name: "staff", weight: 50, parents: [{ kind: "inherit", group: "vip", value: true }], nodes: 2, tracks: [] },
        { name: "default", weight: 0, parents: [], nodes: 1, tracks: ["ranks"] },
        { name: "<b>x</b>", weight: 5, parents: [], nodes: 0, tracks: [] },
        { name: "vip", weight: 10, display_name: "VIP", parents: [{ kind: "inherit", group: "default", value: true }], nodes: 3, tracks: ["ranks"] },
      ],
      tracks: [{ name: "ranks", groups: ["default", "vip"] }],
      vip: { name: "vip", weight: 10, display_name: "VIP", tracks: ["ranks"], revision: 4, nodes: [
        { id: "weight:weight", node: { kind: "weight", weight: 10 } },
        { id: "permission:kit.vip", node: { kind: "permission", key: "kit.vip", value: true } },
        { id: "prefix:100@server=survival", node: { kind: "prefix", priority: 100, text: "&6", contexts: { server: "survival" }, source: "config" } },
      ] },
      user: { uuid: PLAYER, name: "<Alice>", online: true, primary_group: null, effective_primary_group: "vip", tracks: ["ranks"], contexts: { server: "lobby" }, revision: 4,
        nodes: [{ id: "inherit:vip", node: { kind: "inherit", group: "vip", value: true } }] },
      compiled: { subject: { revision: 7, uuid: PLAYER, primary_group: "vip", entries: [{ k: "group", key: "vip", v: true, ctx: {}, rank: 1 }, { k: "perm", key: "kit.vip", v: true, ctx: {}, rank: 1 }], prefix: [{ p: 100, t: "&6", ctx: {}, rank: 1 }], suffix: [], meta: [] },
        contexts: { server: "survival" }, contexts_from: "request", meta: { prefix: "&6", suffix: null, meta: { homes: "5" }, primary_group: "vip" }, groups: ["vip", "default"] },
      ...overrides,
    };
    h.model.intercept = (path, options) => {
      if (!path.startsWith("/api/perms/")) return null;
      const method = options.method || "GET";
      if (method !== "GET" && path !== "/api/perms/trace") {
        calls.push({ path, method, body: JSON.parse(options.body) });
        return h.response(method === "POST" && path === "/api/perms/groups" ? { name: "mod", revision: 5, ops: 1 } : { revision: 5, ops: 1, changed_users: [], changed_groups: [] });
      }
      if (path === "/api/perms/trace") { calls.push({ path, method, body: JSON.parse(options.body) }); return h.response(data.trace); }
      if (path === "/api/perms/groups") return h.response({ revision: 4, groups: data.groups });
      if (path === "/api/perms/tracks") return h.response({ revision: 4, tracks: data.tracks });
      if (path === "/api/perms/groups/vip") return h.response(data.vip);
      if (path.startsWith("/api/perms/users?")) { calls.push({ path, method }); return h.response({ users: [{ uuid: PLAYER, name: "<Alice>", online: true, groups: ["vip"] }], truncated: false }); }
      if (path === `/api/perms/users/${PLAYER}`) return h.response(data.user);
      if (path.startsWith(`/api/perms/users/${PLAYER}/compiled`)) { calls.push({ path, method }); return h.response(data.compiled); }
      if (path === "/api/perms/registered") return h.response({ registered: data.registered || [] });
      if (path.startsWith("/api/perms/search?")) { calls.push({ path, method }); return h.response({ holders: data.holders || [], truncated: false }); }
      if (path.startsWith("/api/perms/audit?")) { calls.push({ path, method }); return h.response({ revision: 9, events: data.events || [] }); }
      return h.response({ error: "unknown group \"ghost\"", code: "not_found" }, 404);
    };
    return calls;
  }
  await test("permission groups render as an inheritance tree and edit groups, nodes and tracks", async () => {
    const h = fixture(appSource), calls = permsModel(h); await flush();
    const rows = h.element("perm-groups").children;
    assert(rows.map((row) => row.children[0].children[0].textContent).join(",") === "default,vip,staff,<b>x</b>", `Tree order wrong: ${rows.map((row) => row.children[0].children[0].textContent)}`);
    assert(rows[1].children[0].className === "tree-depth-1" && rows[2].children[0].className === "tree-depth-2" && rows[3].children[0].className === "tree-depth-0", "Tree depth missing");
    assert(rows[0].children[2].textContent === "Everyone" && rows[2].children[2].textContent === "vip", "Parents column wrong");
    assert(h.element("perm-group-panel").className.includes("hidden"), "Group editor shown before choosing a group");
    await rows[1].children[5].children[0].emit("click"); await flush();
    assert(!h.element("perm-group-panel").className.includes("hidden") && h.element("perm-group-weight").value === "10", "Group editor did not open");
    const nodes = h.element("perm-group-nodes").children;
    assert(nodes.length === 3 && nodes[0].children[6].children.length === 0, "Weight node must be edited with the group");
    assert(nodes[2].children[3].textContent === "server=survival" && nodes[2].children[5].children[0].textContent === "config", "Contexts or source missing");
    // Add a denied node with contexts and an expiry.
    h.element("perm-group-node-kind").value = "permission"; await h.element("perm-group-node-kind").emit("change");
    h.element("perm-group-node-key").value = "Essentials.Fly"; h.element("perm-group-node-grant").value = "false";
    h.element("perm-group-node-contexts").value = "server=survival, region=a; region=b"; h.element("perm-group-node-expiry").value = "3600";
    await h.element("perm-group-node-form").emit("submit"); await flush();
    const added = calls.find((call) => call.path === "/api/perms/groups/vip/nodes");
    assert(added && added.method === "PUT" && added.body.key === "essentials.fly" && added.body.value === false, "Node write incorrect");
    assert(added.body.contexts.server === "survival" && added.body.contexts.region.join() === "a,b", "Contexts not parsed");
    assert(Math.abs(added.body.expires_at - (Date.now() / 1000 + 3600)) < 10, "Expiry not computed");
    // Editing a node replaces it by its id.
    await h.element("perm-group-nodes").children[2].children[6].children[0].emit("click");
    assert(h.element("perm-group-node-kind").value === "prefix" && h.element("perm-group-node-key").value === "100" && h.element("perm-group-node-value").value === "&6", "Edit did not fill the form");
    assert(h.element("perm-group-node-save").textContent === "Save node", "Edit mode not shown");
    h.element("perm-group-node-value").value = "&e";
    await h.element("perm-group-node-form").emit("submit"); await flush();
    const edited = calls.find((call) => call.path === "/api/perms/groups/vip/nodes/prefix%3A100%40server%3Dsurvival");
    assert(edited && edited.body.text === "&e" && edited.body.priority === 100 && edited.body.contexts.server === "survival", "Node edit incorrect");
    const before = calls.length;
    h.element("perm-group-node-kind").value = "permission"; h.element("perm-group-node-key").value = "bad node";
    await h.element("perm-group-node-form").emit("submit"); await flush();
    assert(calls.length === before && h.element("perm-group-node-result").textContent.includes("bad node"), "Invalid node reached the API");
    await h.element("perm-group-nodes").children[1].children[6].children[1].emit("click"); await flush();
    assert(calls.some((call) => call.path === "/api/perms/groups/vip/nodes/permission%3Akit.vip" && call.method === "DELETE"), "Node removal missing");
    // Group attributes and creation.
    h.element("perm-group-weight").value = "15"; h.element("perm-group-display").value = "";
    await h.element("perm-group-form").emit("submit"); await flush();
    const patched = calls.find((call) => call.method === "PATCH");
    assert(patched.path === "/api/perms/groups/vip" && patched.body.weight === 15 && patched.body.display_name === null, "Group patch incorrect");
    h.element("perm-group-create-name").value = "Mod"; h.element("perm-group-create-weight").value = "20";
    await h.element("perm-group-create-form").emit("submit"); await flush();
    const created = calls.find((call) => call.method === "POST" && call.path === "/api/perms/groups");
    assert(created.body.name === "Mod" && created.body.weight === 20 && !("display_name" in created.body), "Group creation incorrect");
    // Tracks reorder with buttons and save the whole list.
    h.element("perm-track-select").value = "ranks"; await h.element("perm-track-select").emit("change");
    const items = h.element("perm-track-groups").children;
    assert(items.map((item) => item.children[0].textContent).join() === "default,vip", "Track groups missing");
    assert(items[0].children[1].children[0].disabled, "The first group cannot move up");
    await items[0].children[1].children[1].emit("click");
    h.element("perm-track-add-group").value = "staff"; await h.element("perm-track-add").emit("click");
    await h.element("perm-track-form").emit("submit"); await flush();
    const track = calls.find((call) => call.path === "/api/perms/tracks/ranks");
    assert(track && track.method === "PUT" && track.body.groups.join() === "vip,default,staff", `Track order not saved: ${JSON.stringify(track)}`);
  });
  await test("players show parents, compiled entries and traces, and promote through the API", async () => {
    const h = fixture(appSource), calls = permsModel(h, { trace: { node: "kit.vip", result: true, winner: { entry: { k: "perm", key: "kit.vip", v: true, ctx: {}, rank: 1 }, specificity: "exact", outcome: "won" },
      losers: [{ entry: { k: "perm", key: "kit.*", v: false, ctx: { server: "lobby" }, rank: 0 }, specificity: "prefix", outcome: "context_mismatch" }], ranks: [{ rank: 0, subject: `user:${PLAYER}` }, { rank: 1, subject: "group:vip" }] } });
    await flush();
    h.element("perm-user-query").value = "alice"; await h.element("perm-user-search-form").emit("submit"); await flush();
    assert(calls.some((call) => call.path === "/api/perms/users?limit=100&q=alice"), "Search request incorrect");
    const row = h.element("perm-users").children[0];
    assert(row.children[0].textContent === "<Alice>" && row.children[2].textContent === "Online", "Player row not rendered safely");
    await row.children[4].children[0].emit("click"); await flush();
    assert(!h.element("perm-user-panel").className.includes("hidden") && h.element("perm-user-heading").textContent === "<Alice>", "Player panel did not open");
    const chip = h.element("perm-user-parents").children[0];
    assert(chip.children[0].textContent === "vip", "Parent chip missing");
    const entries = h.element("perm-compiled-entries").children;
    assert(entries[1].children[0].textContent === "kit.vip" && entries[1].children[3].textContent === "vip · rank 1", "Compiled entries or rank names missing");
    assert(entries[2].children[0].textContent === "prefix 100", "Compiled prefix missing");
    assert(h.element("perm-compiled-meta").children.some((node) => node.textContent === "homes=5"), "Meta view missing");
    h.element("perm-compiled-server").value = "survival"; h.element("perm-compiled-extra").value = "region=spawn";
    await h.element("perm-compiled-form").emit("submit"); await flush();
    assert(calls.some((call) => call.path === `/api/perms/users/${PLAYER}/compiled?server=survival&region=spawn`), "Compiled contexts not sent");
    h.element("perm-trace-node").value = "kit.vip"; await h.element("perm-trace-form").emit("submit"); await flush();
    const traced = calls.find((call) => call.path === "/api/perms/trace");
    assert(traced.body.user === PLAYER && traced.body.contexts.server === "survival" && traced.body.contexts.region === "spawn", "Trace request incorrect");
    assert(h.element("perm-trace-result").textContent.includes("granted"), "Trace result missing");
    const trace = h.element("perm-trace-rows").children;
    assert(trace[0].children[0].textContent === "Wins" && trace[0].children[4].textContent === "group:vip · rank 1" && trace[1].children[0].textContent === "Contexts not active", "Trace rows wrong");
    h.element("perm-user-track").value = "ranks"; await h.element("perm-user-promote").emit("click"); await flush();
    const promoted = calls.find((call) => call.path === "/api/perms/tracks/ranks/promote");
    assert(promoted && promoted.body.user === PLAYER && !promoted.body.contexts, "Promotion request incorrect");
    h.element("perm-user-parent-group").value = "staff"; h.element("perm-user-parent-contexts").value = "server=survival";
    await h.element("perm-user-parent-form").emit("submit"); await flush();
    const parent = calls.find((call) => call.path === `/api/perms/users/${PLAYER}/nodes`);
    assert(parent.body.kind === "inherit" && parent.body.group === "staff" && parent.body.contexts.server === "survival", "Parent add incorrect");
    await h.element("perm-user-parents").children[0].children[1].emit("click"); await flush();
    assert(calls.some((call) => call.path === `/api/perms/users/${PLAYER}/nodes/inherit%3Avip` && call.method === "DELETE"), "Parent removal missing");
    h.element("perm-user-primary").value = "vip"; await h.element("perm-user-primary-form").emit("submit"); await flush();
    assert(calls.some((call) => call.method === "PATCH" && call.path === `/api/perms/users/${PLAYER}` && call.body.primary_group === "vip"), "Primary group not saved");
    // API errors with a code keep their message instead of the generic 404 text.
    h.model.intercept = (path) => path.startsWith("/api/perms/groups") ? h.response({ error: "unknown group \"ghost\"", code: "not_found" }, 404) : null;
    await h.element("perm-groups-refresh").emit("click"); await flush();
    assert(h.element("perm-groups-result").textContent === "unknown group \"ghost\"", `Coded API error replaced: ${h.element("perm-groups-result").textContent}`);
  });
  await test("read-only operators see permissions without forms and others not at all", async () => {
    const h = fixture(appSource); permsModel(h); await flush();
    h.model.access = { name: "viewer", permissions: ["perms"], groups: null };
    h.element("token").value = "viewer-token"; await h.element("auth-form").emit("submit"); await flush();
    assert(!h.element("nav-perm-groups").className.includes("hidden") && h.element("nav-script").className.includes("hidden"), "Permission views not shown to a perms operator");
    for (const id of ["perm-group-create-form", "perm-group-node-form", "perm-user-node-form", "perm-track-form", "perm-user-parent-form", "perm-user-track-form"]) assert(h.element(id).className.includes("hidden"), `${id} shown without config`);
    assert(h.element("perm-groups").children.length === 4, "Groups not listed for a perms operator");
    const reader = fixture(appSource); await flush();
    reader.model.access = { name: "reader", permissions: ["read"], groups: null };
    const before = reader.calls.length;
    reader.element("token").value = "reader-token"; await reader.element("auth-form").emit("submit"); await flush();
    assert(reader.element("section-permissions").className.includes("hidden"), "Permissions section shown without perms");
    assert(!reader.calls.slice(before).some(({ path }) => path.startsWith("/api/perms/")), "Permission data fetched without perms");
  });
  await test("registered nodes group by owner, list holders and activity shows revision diffs", async () => {
    const h = fixture(appSource), calls = permsModel(h, {
      registered: [{ owner: "server.lobby", plugin: "Essentials", node: "essentials.fly", description: "<i>Fly</i>", default: "op" }, { owner: "server.lobby", node: "rift.x", default: "false" }],
      holders: [{ subject: { user: PLAYER }, name: "Alice", id: "permission:essentials.fly@server=survival", node: { kind: "permission", key: "essentials.fly", value: false, contexts: { server: "survival" } } }],
      events: [
        { revision: 8, at: 1, actor: "operator:admin", reason: "tidy", ops: [{ op: "node", subject: { group: "vip" }, before: { kind: "permission", key: "kit.vip", value: true }, after: { kind: "permission", key: "kit.vip", value: false } }], changed: [], groups: ["vip"] },
        { revision: 9, at: 2, actor: "cli", ops: [{ op: "track", name: "ranks", before: null, after: ["default", "vip"] }], changed: [], groups: [] },
      ],
    });
    await flush();
    await h.element("perm-nodes-refresh").emit("click"); await flush();
    const rows = h.element("perm-registered").children;
    assert(rows[0].children[0].textContent === "server.lobby" && rows[2].children[0].textContent === "server.lobby · Essentials", "Owner groups missing");
    assert(rows[3].children[2].textContent === "<i>Fly</i>", "Description not rendered safely");
    h.element("perm-registered-filter").value = "fly"; await h.element("perm-registered-filter").emit("input");
    assert(h.element("perm-registered").children.length === 2, "Filter did not narrow the catalogue");
    await h.element("perm-registered").children[1].children[3].children[0].emit("click"); await flush();
    assert(calls.some((call) => call.path === "/api/perms/search?kind=permission&limit=200&node=essentials.fly"), "Holder search incorrect");
    const holder = h.element("perm-holders").children[0];
    assert(holder.children[0].textContent === `Alice · ${PLAYER}` && holder.children[1].textContent === "Deny" && holder.children[2].textContent === "server=survival", "Holder row wrong");
    h.element("perm-activity-actor").value = "operator:admin"; await h.element("perm-activity-form").emit("submit"); await flush();
    assert(calls.some((call) => call.path === "/api/perms/audit?limit=100&actor=operator%3Aadmin"), "Activity filter not sent");
    let activity = h.element("perm-activity").children;
    assert(activity[0].children[0].textContent === "9" && activity[1].children[0].textContent === "8", "Newest change must come first");
    assert(activity[1].children[4].textContent === "tidy" && activity[1].children[3].textContent.includes("group:vip"), "Change summary missing");
    await activity[1].children[5].children[0].emit("click");
    activity = h.element("perm-activity").children;
    const detail = activity[2].children[0], lines = detail.children[1].children.map((span) => span.textContent);
    assert(detail.children[0].textContent === "Changed group:vip · Permission kit.vip", `Diff title wrong: ${detail.children[0].textContent}`);
    assert(lines.includes("- value: true\n") && lines.includes("+ value: false\n") && lines.includes("  key: kit.vip\n"), `Diff lines wrong: ${lines}`);
    assert(activity[1].children[5].children[0].textContent === "Hide diff", "Diff toggle state missing");
    await activity[0].children[5].children[0].emit("click");
    const track = h.element("perm-activity").children[1].children[0];
    assert(track.children[0].textContent === "Added track:ranks" && track.children[1].children[0].textContent === "+ value: default → vip\n", "Track diff wrong");
  });
  return passed;
}

if (typeof module !== "undefined" && require.main === module) {
  const fs = require("node:fs"), path = require("node:path");
  testWebAssets(fs.readFileSync(path.join(__dirname, "app.js"), "utf8"), fs.readFileSync(path.join(__dirname, "status.js"), "utf8"))
    .then((tests) => { for (const test of tests) console.log(`PASS ${test}`); console.log(`${tests.length} web asset tests passed`); })
    .catch((error) => { console.error(error); process.exitCode = 1; });
}
