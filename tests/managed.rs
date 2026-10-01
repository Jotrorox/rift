//! Real process supervision against a tiny TCP helper in this test executable.
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rift::{
    config::{Config, ManagedServer},
    managed::ManagedServers,
    players::PlayerRegistry,
};

/// Child tests run in an isolated directory; normal suite execution is a no-op.
#[test]
fn managed_process_helper() {
    let Ok(config) = fs::read_to_string("helper-config") else {
        return;
    };
    let mut config = config.lines();
    let address = config.next().unwrap().to_owned();
    let delay: u64 = config.next().unwrap().parse().unwrap();
    let mode = config.next().unwrap();
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open("starts")
            .unwrap(),
        "started"
    )
    .unwrap();
    println!("helper stdout");
    eprintln!("helper stderr");
    if mode == "exit" {
        return;
    }
    std::thread::sleep(Duration::from_millis(delay));
    let listener = TcpListener::bind(&address).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    if mode != "stubborn" {
        let stopping = stopping.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                if line.unwrap_or_default() == "stop" {
                    fs::write("graceful-stop", "yes").unwrap();
                    stopping.store(true, Ordering::Release);
                    break;
                }
            }
        });
    }
    while !stopping.load(Ordering::Acquire) {
        let _ = listener.accept();
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    directory: PathBuf,
    address: SocketAddr,
    config: Config,
    players: Arc<PlayerRegistry>,
}

impl Fixture {
    fn new(delay: u64, mode: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "rift-managed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        fs::write(
            directory.join("helper-config"),
            format!("{address}\n{delay}\n{mode}\n"),
        )
        .unwrap();
        let mut config = Config::from_addresses("127.0.0.1:0", &address.to_string()).unwrap();
        assert!(config.backends.contains_key("default"));
        config.managed_servers.insert(
            "default".to_owned(),
            ManagedServer {
                command: vec![
                    std::env::current_exe()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    "--exact".into(),
                    "managed_process_helper".into(),
                    "--nocapture".into(),
                ],
                directory: directory.clone(),
                autostart: false,
                start_on_connect: true,
                idle_timeout: None,
                start_timeout: Duration::from_secs(3),
                stop_timeout: Duration::from_millis(150),
                restart_delay: Duration::from_millis(200),
            },
        );
        Self {
            directory,
            address,
            config,
            players: Arc::new(PlayerRegistry::default()),
        }
    }

    fn manager(&self) -> ManagedServers {
        let manager = ManagedServers::new(&self.config, self.players.clone());
        manager.launch().unwrap();
        manager
    }

    fn definition(&mut self) -> &mut ManagedServer {
        self.config.managed_servers.get_mut("default").unwrap()
    }
    fn starts(&self) -> usize {
        fs::read_to_string(self.directory.join("starts"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

async fn wait_state(manager: &ManagedServers, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let snapshot = manager.snapshot();
        if snapshot[0].state == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected}, got {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing {}", path.display());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn constructing_without_runtime_has_no_process_side_effects() {
    let mut fixture = Fixture::new(0, "normal");
    fixture.definition().autostart = true;
    let manager = ManagedServers::new(&fixture.config, fixture.players.clone());
    assert_eq!(manager.snapshot()[0].state, "stopped");
    assert_eq!(fixture.starts(), 0);
    assert!(manager.launch().is_err());
}

#[tokio::test]
async fn concurrent_demand_starts_once_and_manual_stop_requires_explicit_restart() {
    let fixture = Fixture::new(150, "normal");
    let manager = fixture.manager();
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let manager = manager.clone();
        tasks.push(tokio::spawn(async move {
            let _lease = manager.reserve("default").unwrap();
            manager.ensure_running("default").await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(fixture.starts(), 1);
    assert_eq!(manager.snapshot()[0].state, "running");
    assert!(manager.snapshot()[0].pid.is_some());
    manager.stop("default").await.unwrap();
    assert!(fixture.directory.join("graceful-stop").exists());
    assert!(!manager.can_connect("default"));
    assert!(manager.reserve("default").is_err());
    assert!(manager.ensure_running("default").await.is_err());
    manager.start("default").await.unwrap();
    assert!(manager.can_connect("default"));
    assert_eq!(fixture.starts(), 2);
    manager.shutdown().await;
    assert_eq!(manager.snapshot()[0].state, "stopped");
    assert!(!manager.can_connect("default"));
    let log = fs::read_to_string(fixture.directory.join("rift-server.log")).unwrap();
    assert!(log.contains("helper stdout") && log.contains("helper stderr"));
}

#[tokio::test]
async fn cancelling_start_waiter_preserves_process_ownership() {
    let fixture = Fixture::new(200, "normal");
    let manager = fixture.manager();
    let task_manager = manager.clone();
    let waiter = tokio::spawn(async move { task_manager.start("default").await });
    wait_file(&fixture.directory.join("starts")).await;
    waiter.abort();
    wait_state(&manager, "running").await;
    assert_eq!(fixture.starts(), 1);
    manager.shutdown().await;
    assert!(fixture.directory.join("graceful-stop").exists());
}

#[tokio::test]
async fn players_and_inflight_leases_prevent_manual_and_idle_stops() {
    let mut fixture = Fixture::new(0, "normal");
    fixture.definition().idle_timeout = Some(Duration::from_millis(150));
    let manager = fixture.manager();
    let lease = manager.reserve("default").unwrap();
    manager.ensure_running("default").await.unwrap();
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(manager.snapshot()[0].state, "running");
    assert!(manager.stop("default").await.is_err());
    let player = fixture
        .players
        .register([1; 16], "Alice", "default")
        .unwrap();
    drop(lease);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(manager.snapshot()[0].state, "running");
    assert!(manager.request_stop("default").is_err());
    drop(player);
    wait_state(&manager, "stopped").await;
    assert!(manager.can_connect("default"));
    let lease = manager.reserve("default").unwrap();
    manager.ensure_running("default").await.unwrap();
    assert_eq!(fixture.starts(), 2);
    drop(lease);
    manager.shutdown().await;
}

#[tokio::test]
async fn readiness_timeout_kills_child_and_failed_start_has_backoff() {
    let mut fixture = Fixture::new(10_000, "normal");
    fixture.definition().start_timeout = Duration::from_millis(150);
    let manager = fixture.manager();
    let error = manager.start("default").await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    let snapshot = manager.snapshot();
    assert_eq!(snapshot[0].state, "failed");
    assert!(snapshot[0].pid.is_none());
    assert!(
        snapshot[0]
            .last_error
            .as_ref()
            .unwrap()
            .contains("timed out")
    );
    assert!(!manager.can_connect("default"));
    assert!(manager.start("default").await.is_err());
    assert_eq!(fixture.starts(), 1);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(manager.can_connect("default"));
    manager.shutdown().await;
}

#[tokio::test]
async fn foreign_listener_is_never_adopted_or_stopped() {
    let fixture = Fixture::new(0, "normal");
    let foreign = TcpListener::bind(fixture.address).unwrap();
    let manager = fixture.manager();
    assert_eq!(
        manager.start("default").await.unwrap_err().kind(),
        std::io::ErrorKind::AddrInUse
    );
    assert_eq!(fixture.starts(), 0);
    manager.shutdown().await;
    assert!(std::net::TcpStream::connect(foreign.local_addr().unwrap()).is_ok());
}

#[tokio::test]
async fn stubborn_process_is_killed_after_graceful_deadline() {
    let fixture = Fixture::new(0, "stubborn");
    let manager = fixture.manager();
    manager.start("default").await.unwrap();
    let start = Instant::now();
    manager.stop("default").await.unwrap();
    assert!(start.elapsed() >= fixture.config.managed_servers["default"].stop_timeout);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    assert!(!fixture.directory.join("graceful-stop").exists());
    manager.shutdown().await;
}

#[tokio::test]
async fn autostart_runs_once_and_idle_shutdown_does_not_restart_it() {
    let mut fixture = Fixture::new(0, "normal");
    fixture.definition().autostart = true;
    fixture.definition().start_on_connect = false;
    fixture.definition().idle_timeout = Some(Duration::from_millis(200));
    let manager = fixture.manager();
    manager.launch().unwrap();
    assert!(!manager.snapshot()[0].automatic_start);
    wait_state(&manager, "running").await;
    wait_state(&manager, "stopped").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fixture.starts(), 1);
    assert!(!manager.can_connect("default"));
    assert!(manager.ensure_running("default").await.is_err());
    manager.shutdown().await;
}

#[tokio::test]
async fn stop_and_shutdown_interrupt_an_in_progress_start() {
    let fixture = Fixture::new(10_000, "normal");
    let manager = fixture.manager();
    manager.request_start("default").unwrap();
    wait_file(&fixture.directory.join("starts")).await;
    let before = Instant::now();
    manager.stop("default").await.unwrap();
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(!manager.can_connect("default"));
    manager.request_start("default").unwrap();
    wait_state(&manager, "starting").await;
    let before = Instant::now();
    manager.shutdown().await;
    assert!(before.elapsed() < Duration::from_secs(1));
    assert_eq!(manager.snapshot()[0].state, "stopped");
}

#[tokio::test]
async fn early_exit_and_missing_command_report_errors_without_restart_loops() {
    let mut fixture = Fixture::new(0, "exit");
    fixture.definition().autostart = true;
    let manager = fixture.manager();
    wait_state(&manager, "failed").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fixture.starts(), 1);
    manager.shutdown().await;
    fixture.definition().autostart = false;
    fixture.definition().command = vec!["/rift-test/definitely-missing-command".into()];
    let manager = fixture.manager();
    assert_eq!(
        manager.start("default").await.unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(manager.snapshot()[0].state, "failed");
    manager.shutdown().await;
}
