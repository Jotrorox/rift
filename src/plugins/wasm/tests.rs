//! Conformance of the WebAssembly engine against real components, driven
//! through the same runtime the proxy uses (`Extensions`, sessions, the
//! worker), never by calling the engine directly.
//!
//! Guests:
//! - `conformance-rs` (`tests/fixtures/plugins/conformance-rs`), the
//!   WebAssembly build of the shared conformance guest, for
//!   [`conformance_tests!`], and `conformance-chat-rs`, the same source
//!   built with its `chat` export, for the scenarios that handle chat.
//! - `hello-rs`, `hello-c` and `hello-go` (`examples/plugins/hello-*`), run
//!   through the shared [`conformance::hello_example`] and the WebAssembly
//!   specifics below. `hello-c` and `hello-go` need a toolchain this test
//!   does not drive: they run when `RIFT_TEST_PLUGIN_WASM_C` or
//!   `RIFT_TEST_PLUGIN_WASM_GO` names the built component, or when the
//!   component sits in its example folder (as CI downloads it).
//! - `probe-rs` (`tests/fixtures/plugins/probe-rs`), a test-only guest for
//!   traps, deadlines, stop requests, capabilities and captured output.
//!
//! `RIFT_TEST_PLUGIN_WASM_DIR` may hold prebuilt `hello_rs.wasm`,
//! `conformance_rs.wasm`, `conformance_chat_rs.wasm` and `probe_rs.wasm`.
//! Otherwise the Rust guests are
//! built here with `cargo build --release --target wasm32-wasip2` into
//! `target/plugin-fixtures`, when that target is installed; without it the
//! scenarios print why they were skipped and pass.
use super::wasi::CAPTURED;
use crate::{
    auth::AuthenticatedProfile,
    config::Config,
    extensions::{Action, ExtensionSession, Extensions, PlayerAction},
    messaging::Broker,
    plugins::conformance::{self, Fixture, conformance_tests},
    plugins::{
        api::{CommandSpec, Hook, Registration, SuggestKind},
        worker::{Engines, Instance, Status},
    },
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::time::timeout;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// A built guest component and the plugin name it is installed under.
#[derive(Clone, Debug)]
pub(crate) struct Guest {
    pub name: String,
    pub wasm: PathBuf,
}

impl Guest {
    /// Install the guest as `plugins/<name>` with `manifest_extra` appended
    /// to a minimal WebAssembly manifest.
    fn install(&self, root: &Path, manifest_extra: &str) {
        let folder = root.join("plugins").join(&self.name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::copy(&self.wasm, folder.join("plugin.wasm")).unwrap();
        std::fs::write(
            folder.join("plugin.toml"),
            format!(
                "name = \"{}\"\nversion = \"0.1.0\"\napi = \"0.1\"\nengine = \"wasm\"\nentry = \"plugin.wasm\"\n{manifest_extra}",
                self.name
            ),
        )
        .unwrap();
    }
}

fn wasm32_wasip2_installed() -> bool {
    let Ok(output) = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(MANIFEST_DIR)
        .output()
    else {
        return false;
    };
    let sysroot = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    output.status.success()
        && Path::new(&sysroot)
            .join("lib/rustlib/wasm32-wasip2/lib")
            .is_dir()
}

/// Build one standalone guest crate; `None` when the target is missing.
fn build(manifest: &str, artifact: &str) -> Option<PathBuf> {
    if let Some(directory) = std::env::var_os("RIFT_TEST_PLUGIN_WASM_DIR") {
        let path = PathBuf::from(directory).join(artifact);
        return path.is_file().then_some(path);
    }
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    if !*INSTALLED.get_or_init(wasm32_wasip2_installed) {
        return None;
    }
    let target = Path::new(MANIFEST_DIR).join("target/plugin-fixtures");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--locked", "--release", "--quiet"])
        .args(["--target", "wasm32-wasip2", "--manifest-path"])
        .arg(Path::new(MANIFEST_DIR).join(manifest))
        .arg("--target-dir")
        .arg(&target)
        .current_dir(MANIFEST_DIR)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .expect("cannot run cargo to build a WebAssembly test guest");
    assert!(status.success(), "building {manifest} failed");
    Some(target.join("wasm32-wasip2/release").join(artifact))
}

/// Every hello fixture available on this machine.
pub(crate) fn hello_guests() -> Vec<Guest> {
    static GUESTS: OnceLock<Vec<Guest>> = OnceLock::new();
    GUESTS
        .get_or_init(|| {
            let mut guests = Vec::new();
            if let Some(wasm) = build("examples/plugins/hello-rs/Cargo.toml", "hello_rs.wasm") {
                guests.push(Guest {
                    name: "hello-rs".into(),
                    wasm,
                });
            }
            for (name, artifact, variable) in [
                ("hello-c", "hello_c.wasm", "RIFT_TEST_PLUGIN_WASM_C"),
                ("hello-go", "hello_go.wasm", "RIFT_TEST_PLUGIN_WASM_GO"),
            ] {
                let built = Path::new(MANIFEST_DIR)
                    .join("examples/plugins")
                    .join(name)
                    .join(artifact);
                let wasm = std::env::var_os(variable)
                    .map(PathBuf::from)
                    .or_else(|| built.is_file().then_some(built));
                match wasm {
                    Some(wasm) => guests.push(Guest {
                        name: name.into(),
                        wasm,
                    }),
                    None => eprintln!(
                        "skipping {name}: set {variable} to its built component to run it"
                    ),
                }
            }
            if guests.is_empty() {
                eprintln!(
                    "skipping WebAssembly conformance: no hello guest (install the wasm32-wasip2 target or set RIFT_TEST_PLUGIN_WASM_DIR)"
                );
            }
            guests
        })
        .clone()
}

fn probe() -> Option<Guest> {
    static PROBE: OnceLock<Option<PathBuf>> = OnceLock::new();
    let wasm = PROBE
        .get_or_init(|| {
            build(
                "tests/fixtures/plugins/probe-rs/Cargo.toml",
                "probe_rs.wasm",
            )
        })
        .clone();
    if wasm.is_none() {
        eprintln!(
            "skipping WebAssembly probe scenarios: probe_rs.wasm unavailable (install the wasm32-wasip2 target or put it in RIFT_TEST_PLUGIN_WASM_DIR)"
        );
    }
    wasm.map(|wasm| Guest {
        name: "probe".into(),
        wasm,
    })
}

fn directory() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rift-wasm-tests-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

const BASE: &str = "listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25001' }, routes = { public = 'lobby' }, authentication = { online_mode = true }, forwarding = { mode = 'velocity', secret_env = 'TEST_SECRET' }";

/// A configuration directory with one installed guest.
fn configure(root: &Path, guest: &Guest, manifest_extra: &str, config: &str) -> Config {
    guest.install(root, manifest_extra);
    let path = root.join("rift.lua");
    std::fs::write(
        &path,
        format!(
            "rift.plugin('{}', {config})\nreturn {{ {BASE} }}",
            guest.name
        ),
    )
    .unwrap();
    Config::load(&path).unwrap()
}

async fn start(config: &Config, broker: &Broker) -> Extensions {
    let runtime =
        Extensions::with_engines(config, broker.clone(), None, Engines::builtin()).unwrap();
    runtime.plugins().start().await.unwrap();
    runtime
}

fn profile(id: u64, name: &str) -> AuthenticatedProfile {
    AuthenticatedProfile {
        uuid: [id as u8; 16],
        name: name.into(),
        properties: vec![],
    }
}

fn session(runtime: &Extensions, id: u64, name: &str) -> ExtensionSession {
    let mut context = crate::extensions::Context::authenticated(id, &profile(id, name));
    context.default_server = Some("lobby".into());
    runtime.session(context).unwrap()
}

fn grant(runtime: &Extensions, id: u64, node: &str) {
    let uuid = format!("{:02x}", id as u8).repeat(16);
    runtime
        .plugins()
        .persistent
        .base
        .permissions
        .grant(&uuid, node);
}

fn instance(runtime: &Extensions) -> Arc<Instance> {
    runtime.plugins().active()[0].clone()
}

async fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn captured(prefix: &str) -> Vec<String> {
    CAPTURED
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.starts_with(prefix))
        .cloned()
        .collect()
}

/// Every hello scenario against one guest.
pub(crate) async fn hello_conformance(guest: &Guest) {
    let root = directory();
    let config = configure(&root, guest, "", "{ deny = 'Mallory' }");
    let broker = Broker::default();
    let runtime = start(&config, &broker).await;
    let plugin = instance(&runtime);
    assert_eq!(plugin.status(), Status::Running, "{}", guest.name);

    // init: the registration.
    assert_eq!(
        *plugin.registration().unwrap(),
        Registration {
            chat: false,
            commands: vec![CommandSpec {
                name: "hello".into(),
                permission: "hello.use".into(),
                suggest: SuggestKind::Players,
            }],
            hooks: vec![
                Hook::Login,
                Hook::Command,
                Hook::Join,
                Hook::Message,
                Hook::Timer
            ],
        }
    );

    // login: deny exactly the configured name.
    assert_eq!(
        session(&runtime, 1, "Mallory")
            .decision("login")
            .await
            .unwrap(),
        Action::Deny("You are not welcome here.".into())
    );
    assert_eq!(
        session(&runtime, 2, "Alex")
            .decision("login")
            .await
            .unwrap(),
        Action::Continue
    );
    // A hook the plugin did not register never reaches it.
    let calls = plugin.report().calls;
    assert_eq!(
        session(&runtime, 2, "Alex")
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Continue
    );
    assert_eq!(plugin.report().calls, calls);

    // command: needs hello.use, then replies with the player's name.
    let mut player = session(&runtime, 2, "Alex");
    assert_eq!(
        player.command("hello").await.unwrap(),
        Action::Message("You do not have permission to use this command.".into())
    );
    grant(&runtime, 2, "hello.use");
    assert_eq!(player.visible_commands(), [("hello".to_owned(), true)]);
    assert_eq!(
        player.command("hello").await.unwrap(),
        Action::Message("Hello, Alex!".into())
    );

    // join: a welcome message through players.message and a store counter.
    let (handle, mut actions) =
        runtime
            .players()
            .register(2, &profile(2, "Alex").uuid, "Alex", 774, "lobby");
    player.ready("lobby").await;
    assert_eq!(
        timeout(Duration::from_secs(5), actions.recv())
            .await
            .unwrap()
            .unwrap(),
        PlayerAction::Message("Welcome, Alex!".into())
    );
    let namespace = format!("plugin.{}.hello", guest.name);
    let store = runtime.plugins().persistent.base.store.clone();
    wait_for("the join counter", Duration::from_secs(5), || {
        store.namespace(&namespace) == [("joins".to_owned(), b"1".to_vec())]
    })
    .await;

    // suggest = players: online names, without calling the plugin.
    let calls = plugin.report().calls;
    let suggestions = player.suggest("hello", "Al", Vec::new).await.unwrap();
    let names: Vec<_> = suggestions.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(names, ["Alex"]);
    assert_eq!(plugin.report().calls, calls);
    drop(handle);

    // bus: hello.ping replies with the request payload.
    let reply = broker
        .request(
            "hello.ping",
            bytes::Bytes::from_static(b"ping \x00 payload"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(&reply.payload[..], b"ping \x00 payload");

    // timer: the 1000 ms ticker keeps calling the plugin.
    let calls = plugin.report().calls;
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(
        plugin.report().calls >= calls + 2,
        "{}: timer ticks did not arrive",
        guest.name
    );

    assert_eq!(plugin.status(), Status::Running);
    assert_eq!(plugin.report().restarts, 0);
    runtime.plugins().shutdown().await;
    assert_eq!(plugin.status(), Status::Stopped);
}

#[tokio::test]
async fn hello_guests_conform() {
    for guest in hello_guests() {
        hello_conformance(&guest).await;
    }
}

/// The example folder as shipped (its own `plugin.toml`) with the built
/// component under the manifest's `entry`.
fn example_folder(guest: &Guest) -> PathBuf {
    let example = Path::new(MANIFEST_DIR)
        .join("examples/plugins")
        .join(&guest.name);
    let folder = directory().join(&guest.name);
    std::fs::create_dir_all(&folder).unwrap();
    let manifest = std::fs::read_to_string(example.join("plugin.toml")).unwrap();
    let entry = crate::plugins::manifest::Manifest::parse(&manifest, &guest.name)
        .unwrap()
        .entry
        .unwrap();
    std::fs::write(folder.join("plugin.toml"), manifest).unwrap();
    std::fs::copy(&guest.wasm, folder.join(entry)).unwrap();
    folder
}

#[tokio::test]
async fn example_plugins_follow_the_shared_spec() {
    for guest in hello_guests() {
        conformance::hello_example(Engines::builtin(), &example_folder(&guest)).await;
    }
}

fn conformance_guest() -> Option<PathBuf> {
    static GUEST: OnceLock<Option<PathBuf>> = OnceLock::new();
    GUEST
        .get_or_init(|| {
            let wasm = build(
                "tests/fixtures/plugins/conformance-rs/Cargo.toml",
                "conformance_rs.wasm",
            );
            if wasm.is_none() {
                eprintln!(
                    "skipping the shared conformance scenarios for wasm: conformance_rs.wasm unavailable (install the wasm32-wasip2 target or put it in RIFT_TEST_PLUGIN_WASM_DIR)"
                );
            }
            wasm
        })
        .clone()
}

/// The conformance guest built with its `chat` export
/// (`tests/fixtures/plugins/conformance-chat-rs`).
fn conformance_chat_guest() -> Option<PathBuf> {
    static GUEST: OnceLock<Option<PathBuf>> = OnceLock::new();
    GUEST
        .get_or_init(|| {
            let wasm = build(
                "tests/fixtures/plugins/conformance-chat-rs/Cargo.toml",
                "conformance_chat_rs.wasm",
            );
            if wasm.is_none() {
                eprintln!(
                    "skipping the shared conformance scenarios for wasm: conformance_chat_rs.wasm unavailable (install the wasm32-wasip2 target or put it in RIFT_TEST_PLUGIN_WASM_DIR)"
                );
            }
            wasm
        })
        .clone()
}

fn install_conformance(wasm: PathBuf, folder: &Path, name: &str, extra: &str) {
    std::fs::copy(wasm, folder.join("plugin.wasm")).unwrap();
    std::fs::write(
        folder.join("plugin.toml"),
        format!(
            "name = \"{name}\"\nversion = \"1\"\napi = \"0.1\"\nengine = \"wasm\"\nentry = \"plugin.wasm\"\n{extra}"
        ),
    )
    .unwrap();
}

/// The conformance guest as a WebAssembly plugin folder.
struct WasmFixture;

impl Fixture for WasmFixture {
    fn engines(&self) -> Engines {
        Engines::builtin()
    }

    fn install(&self, folder: &Path, name: &str, extra: &str) {
        let wasm = conformance_guest().expect("conformance_rs.wasm");
        install_conformance(wasm, folder, name, extra);
    }

    fn install_chat(&self, folder: &Path, name: &str, extra: &str) {
        let wasm = conformance_chat_guest().expect("conformance_chat_rs.wasm");
        install_conformance(wasm, folder, name, extra);
    }
}

mod shared {
    use super::*;

    conformance_tests!(
        WasmFixture,
        if conformance_guest().is_some() && conformance_chat_guest().is_some()
    );
}

#[tokio::test]
async fn a_trap_fails_the_instance_and_it_restarts() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "", "{}");
    let broker = Broker::default();
    let mut events = broker
        .subscribe(crate::plugins::worker::EVENTS, None)
        .unwrap();
    let runtime = start(&config, &broker).await;
    let plugin = instance(&runtime);
    let error = session(&runtime, 1, "Trap")
        .decision("login")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("trap"), "{error}");
    // The reply goes out before the worker records the failure.
    wait_for("the failure", Duration::from_secs(1), || {
        plugin.report().last_error.is_some()
    })
    .await;
    let failure = plugin.report().last_error.unwrap();
    assert!(failure.contains("login: trap: "), "{failure}");
    // The panic message reached the plugin log through stderr.
    assert!(
        captured("probe: stderr: ")
            .iter()
            .any(|line| line.contains("probe trap")),
        "{:?}",
        captured("probe: ")
    );
    wait_for("the restart", Duration::from_secs(5), || {
        plugin.status() == Status::Running
    })
    .await;
    assert_eq!(plugin.report().restarts, 1);
    assert_eq!(
        session(&runtime, 2, "Alex")
            .decision("login")
            .await
            .unwrap(),
        Action::Continue
    );
    let mut seen = Vec::new();
    while let Ok(Some(message)) = events.try_recv() {
        let value: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
        seen.push(value["event"].as_str().unwrap().to_owned());
    }
    assert_eq!(seen, ["started", "failed", "restarting", "started"]);
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn a_guest_past_its_deadline_is_interrupted_and_restarted() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "[deadlines]\nlogin = 100\n", "{}");
    let broker = Broker::default();
    let runtime = start(&config, &broker).await;
    let plugin = instance(&runtime);
    let started = Instant::now();
    // on_timeout defaults to deny for login.
    assert!(
        session(&runtime, 1, "Spin")
            .decision("login")
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    // The guest itself is stopped at the epoch after the deadline.
    wait_for("the interruption", Duration::from_secs(2), || {
        plugin.report().last_error.is_some()
    })
    .await;
    let error = plugin.report().last_error.unwrap();
    assert!(error.contains("login exceeded 100 ms"), "{error}");
    wait_for("the restart", Duration::from_secs(5), || {
        plugin.status() == Status::Running
    })
    .await;
    assert_eq!(
        session(&runtime, 2, "Alex")
            .decision("login")
            .await
            .unwrap(),
        Action::Continue
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn shutdown_interrupts_a_stuck_guest() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "[deadlines]\nshutdown = 200\n", "{}");
    let broker = Broker::default();
    let runtime = start(&config, &broker).await;
    let plugin = instance(&runtime);
    // Observers run up to 60 s: this join outlasts the shutdown grace.
    let mut player = session(&runtime, 1, "Spin");
    player.ready("lobby").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    runtime.plugins().shutdown().await;
    timeout(Duration::from_secs(2), plugin.exited())
        .await
        .expect("the stop request did not interrupt the guest");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(plugin.status(), Status::Stopped);
}

#[derive(serde::Deserialize)]
struct Report {
    data: bool,
    host: bool,
    host_writable: bool,
    meta_data: String,
    meta_config: Option<String>,
    env: Vec<String>,
    network: String,
}

async fn probe_report(runtime: &Extensions, args: &str) -> Report {
    grant(runtime, 1, "probe.use");
    let Action::Message(text) = session(runtime, 1, "Alex")
        .command(&format!("probe {args}"))
        .await
        .unwrap()
    else {
        panic!("probe did not reply with a message");
    };
    serde_json::from_str(&text.to_string()).unwrap()
}

#[tokio::test]
async fn capabilities_limit_what_the_guest_sees() {
    let Some(guest) = probe() else { return };
    let broker = Broker::default();

    let root = directory();
    let config = configure(
        &root,
        &guest,
        "[capabilities]\nnetwork = false\nenv = [\"PATH\", \"RIFT_UNSET_VARIABLE\"]\n",
        "{}",
    );
    let runtime = start(&config, &broker).await;
    let report = probe_report(&runtime, "sandboxed").await;
    assert!(report.data, "the data directory is preopened read-write");
    assert!(
        root.join("plugins/probe/data/probe.txt").is_file(),
        "/data is the plugin's data directory"
    );
    assert!(!report.host, "/host is not visible without the grant");
    assert_eq!(report.meta_data, "/data");
    assert_eq!(report.meta_config, None);
    assert_eq!(report.env, ["PATH"], "listed variables that are set");
    let denied = report.network;
    assert!(denied.contains("Denied"), "{denied}");
    runtime.plugins().shutdown().await;
    // stdout reaches the plugin log line by line, and the unterminated rest
    // when the instance ends.
    let lines = captured("probe: stdout: ");
    assert!(
        lines.contains(&"probe: stdout: probe stdout sandboxed".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"probe: stdout: partial".to_owned()),
        "{lines:?}"
    );

    let root = directory();
    let config = configure(
        &root,
        &guest,
        // A refused connection can take seconds on Windows.
        "[deadlines]\ncommand = 10000\n[capabilities]\nfilesystem = [\"data\", \"host\"]\nnetwork = true\nenv = []\n",
        "{}",
    );
    let runtime = start(&config, &broker).await;
    let report = probe_report(&runtime, "granted").await;
    assert!(report.data);
    assert!(report.host);
    // The configuration directory is visible but cannot be changed.
    assert!(!report.host_writable);
    assert!(!root.join("probe-write.txt").exists());
    assert_eq!(report.meta_config.as_deref(), Some("/host"));
    assert!(report.env.is_empty(), "only listed variables are passed");
    assert_ne!(report.network, denied, "network grants sockets");
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn init_and_reload_errors_are_handler_errors() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "", "{ fail = 'not today' }");
    let broker = Broker::default();
    let runtime =
        Extensions::with_engines(&config, broker.clone(), None, Engines::builtin()).unwrap();
    let error = runtime.plugins().start().await.unwrap_err();
    assert!(error.to_string().contains("init: not today"), "{error}");
    runtime.plugins().shutdown().await;

    let root = directory();
    let config = configure(&root, &guest, "", "{}");
    let runtime = start(&config, &broker).await;
    let plugin = instance(&runtime);
    let error = plugin
        .reload(r#"{"fail":"bad config"}"#.into())
        .await
        .unwrap_err();
    assert_eq!(error, "bad config");
    assert_eq!(plugin.status(), Status::Running);
    plugin.reload("{}".into()).await.unwrap();
    assert_eq!(plugin.config_json(), "{}");
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn invalid_artifacts_fail_with_a_reason() {
    let root = directory();
    let guest = Guest {
        name: "broken".into(),
        wasm: root.join("broken.wasm"),
    };
    // A core module, not a component.
    std::fs::write(&guest.wasm, b"\0asm\x01\0\0\0").unwrap();
    let config = configure(&root, &guest, "[failure]\nrestart = \"never\"\n", "{}");
    let runtime =
        Extensions::with_engines(&config, Broker::default(), None, Engines::builtin()).unwrap();
    let error = runtime.plugins().start().await.unwrap_err();
    assert!(
        error.to_string().contains("is not a WebAssembly component"),
        "{error}"
    );
    runtime.plugins().shutdown().await;
    assert!(super::precompile(&config.plugins[0]).is_err());
}

/// `rift check --strict` refuses a component that exports `chat` while chat
/// is signed, from its exports alone; the ordinary build passes either way.
#[tokio::test]
async fn precompiling_a_chat_component_needs_unsigned_chat() {
    let (Some(chat), Some(plain)) = (conformance_chat_guest(), conformance_guest()) else {
        return;
    };
    let root = directory();
    let folder = root.join("plugins/mute");
    std::fs::create_dir_all(&folder).unwrap();
    install_conformance(chat, &folder, "mute", "");
    let mut spec = conformance::installed(&folder, "{}");
    assert!(spec.signed_chat);
    let error = super::precompile(&spec).unwrap_err();
    assert!(
        error.contains(
            "plugin.wasm exports chat: the chat hook needs authentication.signed_chat = false"
        ),
        "{error}"
    );
    spec.signed_chat = false;
    super::precompile(&spec).unwrap();

    let folder = root.join("plugins/plain");
    std::fs::create_dir_all(&folder).unwrap();
    install_conformance(plain, &folder, "plain", "");
    super::precompile(&conformance::installed(&folder, "{}")).unwrap();
}

#[tokio::test]
async fn precompiled_components_are_loaded_from_the_cache() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "", "{}");
    let spec = &config.plugins[0];
    let note = super::precompile(spec).unwrap();
    assert!(
        note.replace('\\', "/").contains("local/plugincache/probe-"),
        "{note}"
    );
    let (_, key) = super::read(spec).unwrap();
    let path = super::cache::path(&root, "probe", &key);
    let engine = super::engine().unwrap();
    assert!(super::cache::load(engine, &path, &key).is_some());
    // Another key or a damaged file is never deserialized.
    assert!(super::cache::load(engine, &path, &"0".repeat(32)).is_none());
    let original = std::fs::read(&path).unwrap();
    let mut damaged = original.clone();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 1;
    std::fs::write(&path, &damaged).unwrap();
    assert!(super::cache::load(engine, &path, &key).is_none());
    // Knowing the key is not enough: a checksum anyone can compute, or a
    // MAC under another secret, does not make a file loadable.
    let contents = &original[..original.len() - 32];
    let mut forged = contents.to_vec();
    {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(key.as_bytes());
        hash.update([0]);
        hash.update(contents);
        forged.extend_from_slice(&hash.finalize());
    }
    std::fs::write(&path, &forged).unwrap();
    assert!(super::cache::load(engine, &path, &key).is_none());
    let secret = root.join("local/plugincache/key");
    let elsewhere = directory();
    super::cache::store(
        &super::cache::path(&elsewhere, "probe", &key),
        "probe",
        &key,
        contents,
    )
    .unwrap();
    std::fs::copy(super::cache::path(&elsewhere, "probe", &key), &path).unwrap();
    assert!(super::cache::load(engine, &path, &key).is_none());
    std::fs::write(&path, &original).unwrap();
    assert!(super::cache::load(engine, &path, &key).is_some());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A secret others can read no longer authenticates anything.
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(super::cache::load(engine, &path, &key).is_none());
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // Startup still works.
    let runtime = start(&config, &Broker::default()).await;
    assert_eq!(instance(&runtime).status(), Status::Running);
    runtime.plugins().shutdown().await;
}

/// Not a correctness test: `cargo test --release --lib wasm_timings --
/// --ignored --nocapture` prints compile, cache and per-hook timings.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn wasm_timings() {
    let Some(guest) = hello_guests().into_iter().next() else {
        return;
    };
    let root = directory();
    let config = configure(&root, &guest, "", "{ deny = 'Mallory' }");
    let spec = &config.plugins[0];
    let (wasm, key) = super::read(spec).unwrap();
    let engine = super::engine().unwrap();
    let started = Instant::now();
    let component = super::compile(spec, &wasm).unwrap();
    let compiled = started.elapsed();
    let path = super::cache::path(&root, &spec.name, &key);
    super::cache::store(&path, &spec.name, &key, &component.serialize().unwrap()).unwrap();
    let started = Instant::now();
    let component = super::cache::load(engine, &path, &key).unwrap();
    let deserialized = started.elapsed();
    let started = Instant::now();
    super::prepare(spec, &component).unwrap();
    let linked = started.elapsed();
    let broker = Broker::default();
    // The first start fills the in-memory cache from disk; time the second,
    // which only instantiates and runs init.
    start(&config, &broker).await.plugins().shutdown().await;
    let started = Instant::now();
    let runtime = start(&config, &broker).await;
    let instantiated = started.elapsed();
    grant(&runtime, 2, "hello.use");
    let player = session(&runtime, 2, "Alex");
    for _ in 0..200 {
        player.decision("login").await.unwrap();
    }
    let rounds = 5000;
    let started = Instant::now();
    for _ in 0..rounds {
        player.decision("login").await.unwrap();
    }
    let login = started.elapsed() / rounds;
    let started = Instant::now();
    for _ in 0..rounds {
        player.command("hello").await.unwrap();
    }
    let command = started.elapsed() / rounds;
    eprintln!(
        "{}: {} KiB component; cold compile {compiled:?}; cache load {deserialized:?} + link {linked:?}; start with a cached component (instantiate + init) {instantiated:?}; login round trip {login:?}; command round trip {command:?}",
        guest.name,
        wasm.len() / 1024
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn the_memory_limit_applies_to_each_instance() {
    let Some(guest) = probe() else { return };
    let broker = Broker::default();
    let root = directory();
    let config = configure(
        &root,
        &guest,
        "[failure]\nrestart = \"never\"\n[wasm]\nmemory_limit_mb = 1\n",
        "{}",
    );
    let runtime =
        Extensions::with_engines(&config, broker.clone(), None, Engines::builtin()).unwrap();
    let error = runtime.plugins().start().await.unwrap_err().to_string();
    assert!(error.contains("memory"), "{error}");
    runtime.plugins().shutdown().await;
    let root = directory();
    let config = configure(&root, &guest, "[wasm]\nmemory_limit_mb = 16\n", "{}");
    let runtime = start(&config, &broker).await;
    assert_eq!(instance(&runtime).status(), Status::Running);
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn components_show_dialogs_and_receive_clicks_without_registering_a_hook() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "", "{}");
    let runtime = start(&config, &Broker::default()).await;
    let (_handle, mut actions) = runtime
        .players()
        .register(31, &[31; 16], "Dialog", 774, "lobby");
    let player = session(&runtime, 31, "Dialog");
    assert_eq!(player.decision("login").await.unwrap(), Action::Continue);
    let Ok(PlayerAction::Dialog { dialog, owner }) = actions.try_recv() else {
        panic!("expected a dialog");
    };
    assert_eq!(owner, crate::dialog::Owner::Plugin(guest.name.clone()));
    let id = dialog.wire()["action"]["action"]["id"].as_str().unwrap();
    let (response, _) = dialog
        .resolve(id, Some(serde_json::json!({"answer": "42"})))
        .unwrap();
    player.dialog(owner, response).await.unwrap();
    let message = timeout(Duration::from_secs(5), actions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        message,
        PlayerAction::Message(r#"reply {"answer":"42"} {"round":1}"#.into())
    );
    assert_eq!(instance(&runtime).status(), Status::Running);
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn styled_components_answer_with_rich_text_and_titles() {
    use crate::text::Text;
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "", "{}");
    let runtime = start(&config, &Broker::default()).await;
    grant(&runtime, 1, "probe.use");

    // `styled.suggest` replaces `handler.suggest`; tooltips are components.
    let suggestions = session(&runtime, 1, "Alex")
        .suggest("probe", "", Vec::new)
        .await
        .unwrap();
    let texts: Vec<_> = suggestions.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["rich", "plain"]);
    let Some(Text::Component(tooltip)) = &suggestions[0].tooltip else {
        panic!("expected a component tooltip: {:?}", suggestions[0].tooltip);
    };
    let tooltip = tooltip.to_string();
    assert!(
        tooltip.contains("Alex") && !tooltip.contains("mini"),
        "{tooltip}"
    );
    assert_eq!(suggestions[1].tooltip, None);
    let bad = session(&runtime, 1, "Alex")
        .suggest("probe", "bad", Vec::new)
        .await;
    assert!(
        bad.as_ref().map_or(true, Vec::is_empty),
        "an invalid tooltip offers nothing: {bad:?}"
    );

    // `styled.status` replaces `handler.status`.
    let ping = |hostname: &str| crate::server_list::Ping {
        listener: "public".into(),
        hostname: hostname.into(),
        protocol: 774,
        peer_addr: "127.0.0.1:5000".parse().unwrap(),
        backend: None,
        maintenance: false,
        online: 0,
        max_players: 20,
    };
    let status = runtime
        .plugins()
        .status(&ping("rich.example"))
        .await
        .unwrap()
        .expect("an override");
    let Some(Text::Component(version)) = &status.version else {
        panic!("expected a component version: {:?}", status.version);
    };
    assert!(version.to_string().contains("Probe"), "{version}");
    assert_eq!(status.sample.as_ref().map(Vec::len), Some(2));
    assert_eq!(
        runtime
            .plugins()
            .status(&ping("other.example"))
            .await
            .unwrap(),
        None
    );
    assert!(
        !matches!(
            runtime.plugins().status(&ping("bad.example")).await,
            Ok(Some(_))
        ),
        "an invalid version is not applied"
    );

    // `players.title-json` with markup and legacy codes.
    let (_handle, mut actions) = runtime
        .players()
        .register(41, &[41; 16], "Title", 774, "lobby");
    let player = session(&runtime, 41, "Title");
    assert_eq!(player.decision("login").await.unwrap(), Action::Continue);
    let Ok(PlayerAction::Title {
        title: Some(title),
        subtitle: Some(subtitle),
        times,
    }) = actions.try_recv()
    else {
        panic!("expected a title");
    };
    assert_eq!(times, [5, 40, 10]);
    assert!(format!("{title:?}").contains("Welcome"), "{title:?}");
    assert!(
        format!("{subtitle:?}").contains("to the probe"),
        "{subtitle:?}"
    );
    runtime.plugins().shutdown().await;
}

/// `players.locale` (rift:plugin 0.1.5): `none` until the session publishes
/// the client's language, then what the client last said.
#[tokio::test]
async fn components_read_the_client_locale_once_it_is_known() {
    let Some(guest) = probe() else { return };
    let root = directory();
    let config = configure(&root, &guest, "", "{}");
    let runtime = start(&config, &Broker::default()).await;
    let (handle, _actions) = runtime
        .players()
        .register(42, &[42; 16], "Locale", 774, "lobby");
    let player = session(&runtime, 42, "Locale");
    assert_eq!(
        player.decision("login").await.unwrap(),
        Action::Deny("locale=unknown".into())
    );
    for locale in ["de_de", "en_us"] {
        handle.set_locale(Some(locale));
        assert_eq!(
            player.decision("login").await.unwrap(),
            Action::Deny(format!("locale={locale}").as_str().into())
        );
    }
    // Offline players have none either.
    drop(handle);
    assert_eq!(
        player.decision("login").await.unwrap(),
        Action::Deny("locale=unknown".into())
    );
    runtime.plugins().shutdown().await;
}
