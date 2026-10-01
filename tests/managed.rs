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
    if mode == "exit"
        || (mode == "fail-first" && fs::read_to_string("starts").unwrap().lines().count() == 1)
    {
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
    let ready_at = Instant::now();
    while !stopping.load(Ordering::Acquire) {
        if fs::remove_file("crash").is_ok()
            || (mode == "flap" && ready_at.elapsed() >= Duration::from_millis(200))
        {
            return;
        }
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
                restart_retries: 3,
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
    fixture.definition().restart_retries = 0;
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
    assert!(!manager.can_connect("default"));
    assert!(manager.snapshot()[0].restart_exhausted);
    assert_eq!(
        manager.start("default").await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert_eq!(fixture.starts(), 2);
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
async fn early_exit_and_missing_command_report_errors_with_bounded_retries() {
    let mut fixture = Fixture::new(0, "exit");
    fixture.definition().autostart = true;
    let manager = fixture.manager();
    wait_exhausted(&manager).await;
    assert_eq!(fixture.starts(), 4);
    assert_eq!(manager.snapshot()[0].restart_attempts, 3);
    assert!(!manager.can_connect("default"));
    assert!(manager.ensure_running("default").await.is_err());
    assert!(manager.request_scale_start("default").is_err());
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(fixture.starts(), 4);
    // An administrator can explicitly retry an exhausted service.
    assert!(manager.start("default").await.is_err());
    assert_eq!(fixture.starts(), 5);
    assert_eq!(manager.snapshot()[0].restart_attempts, 0);
    manager.shutdown().await;
    fixture.definition().autostart = false;
    fixture.definition().command = vec!["/rift-test/definitely-missing-command".into()];
    fixture.definition().restart_delay = Duration::from_millis(50);
    let manager = fixture.manager();
    assert_eq!(
        manager.start("default").await.unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert_eq!(manager.snapshot()[0].state, "failed");
    wait_exhausted(&manager).await;
    assert_eq!(manager.snapshot()[0].restart_attempts, 3);
    assert_eq!(fixture.starts(), 5);
    manager.shutdown().await;
}

async fn wait_exhausted(manager: &ManagedServers) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !manager.snapshot()[0].restart_exhausted {
        assert!(
            Instant::now() < deadline,
            "recovery did not exhaust: {:?}",
            manager.snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn readiness_timeouts_retry_only_within_budget_and_reap_each_child() {
    let mut fixture = Fixture::new(10_000, "normal");
    fixture.definition().start_timeout = Duration::from_millis(100);
    fixture.definition().restart_delay = Duration::from_millis(50);
    fixture.definition().restart_retries = 1;
    let manager = fixture.manager();
    assert_eq!(
        manager.start("default").await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    wait_exhausted(&manager).await;
    assert_eq!(fixture.starts(), 2);
    assert!(manager.snapshot()[0].pid.is_none());
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.starts(), 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn failed_autostart_recovers_without_connection_demand() {
    let mut fixture = Fixture::new(0, "fail-first");
    fixture.definition().autostart = true;
    fixture.definition().start_on_connect = false;
    let manager = fixture.manager();
    wait_state(&manager, "running").await;
    assert_eq!(fixture.starts(), 2);
    assert_eq!(manager.snapshot()[0].restart_attempts, 1);
    assert!(manager.snapshot()[0].last_error.is_none());
    manager.shutdown().await;
}

#[tokio::test]
async fn unexpected_exit_recovers_after_delay_and_releases_owned_pid() {
    let mut fixture = Fixture::new(0, "normal");
    fixture.definition().start_on_connect = false;
    fixture.definition().restart_delay = Duration::from_millis(400);
    let manager = fixture.manager();
    manager.request_scale_start("default").unwrap();
    wait_state(&manager, "running").await;
    let original_pid = manager.snapshot()[0].pid;
    let player = fixture
        .players
        .register([9; 16], "Recovering", "default")
        .unwrap();
    fs::write(fixture.directory.join("crash"), "yes").unwrap();
    wait_state(&manager, "failed").await;
    assert!(manager.snapshot()[0].pid.is_none());
    assert!(
        manager.snapshot()[0]
            .last_error
            .as_ref()
            .unwrap()
            .contains("exited")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.starts(), 1);
    wait_state(&manager, "running").await;
    assert_eq!(fixture.starts(), 2);
    assert_ne!(manager.snapshot()[0].pid, original_pid);
    assert_eq!(manager.snapshot()[0].restart_attempts, 1);
    assert!(manager.stop("default").await.is_err());
    drop(player);
    manager.stop("default").await.unwrap();
    assert!(manager.request_scale_start("default").is_err());
    assert!(!manager.snapshot()[0].automatic_enabled);
    manager.shutdown().await;
}

#[tokio::test]
async fn briefly_ready_crashing_services_cannot_reset_restart_budget() {
    let mut fixture = Fixture::new(0, "flap");
    fixture.definition().restart_retries = 2;
    let manager = fixture.manager();
    manager.start("default").await.unwrap();
    wait_exhausted(&manager).await;
    assert_eq!(fixture.starts(), 3);
    assert_eq!(manager.snapshot()[0].restart_attempts, 2);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(fixture.starts(), 3);
    manager.shutdown().await;
}

#[tokio::test]
async fn administrative_stop_shutdown_and_removal_cancel_pending_recovery() {
    for action in ["stop", "shutdown", "remove"] {
        let mut fixture = Fixture::new(0, "exit");
        fixture.definition().restart_delay = Duration::from_millis(500);
        let manager = fixture.manager();
        assert!(manager.start("default").await.is_err());
        assert_eq!(fixture.starts(), 1);
        match action {
            "stop" => manager.stop("default").await.unwrap(),
            "remove" => manager.remove("default").await.unwrap(),
            _ => manager.shutdown().await,
        }
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert_eq!(fixture.starts(), 1, "{action} did not cancel recovery");
        manager.shutdown().await;
    }
}

fn empty_manager(fixture: &Fixture) -> ManagedServers {
    let mut empty = fixture.config.clone();
    empty.managed_servers.clear();
    let manager = ManagedServers::new(&empty, fixture.players.clone());
    manager.launch().unwrap();
    manager
}

async fn wait_removed(manager: &ManagedServers) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !manager.snapshot().is_empty() {
        assert!(Instant::now() < deadline, "server did not retire");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn dynamic_registration_autostarts_and_removal_reaps_before_reuse() {
    let mut fixture = Fixture::new(0, "normal");
    fixture.definition().autostart = true;
    let manager = empty_manager(&fixture);
    assert!(manager.snapshot().is_empty());
    manager.add(&fixture.config, "default").unwrap();
    assert_eq!(
        manager.add(&fixture.config, "default").unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    wait_state(&manager, "running").await;
    assert_eq!(fixture.starts(), 1);
    manager.remove("default").await.unwrap();
    assert!(manager.snapshot().is_empty());
    assert!(fixture.directory.join("graceful-stop").exists());
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    // Stale routing snapshots must not turn a deleted managed backend into an
    // unmanaged destination that can bypass the lifecycle admission guard.
    assert!(manager.is_managed("default"));
    assert!(!manager.can_connect("default"));
    assert!(manager.reserve("default").is_err());
    assert!(manager.ensure_running("default").await.is_err());
    assert!(manager.start("default").await.is_err());
    manager.add(&fixture.config, "default").unwrap();
    wait_state(&manager, "running").await;
    assert!(manager.can_connect("default"));
    assert_eq!(fixture.starts(), 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn removal_refuses_players_and_pending_connections_without_disabling_backend() {
    let fixture = Fixture::new(0, "normal");
    let manager = empty_manager(&fixture);
    manager.add(&fixture.config, "default").unwrap();
    let lease = manager.reserve("default").unwrap();
    manager.ensure_running("default").await.unwrap();
    assert_eq!(
        manager.remove("default").await.unwrap_err().kind(),
        std::io::ErrorKind::ResourceBusy
    );
    assert!(manager.can_connect("default"));
    let player = fixture.players.register([2; 16], "Bob", "default").unwrap();
    drop(lease);
    assert_eq!(
        manager.remove("default").await.unwrap_err().kind(),
        std::io::ErrorKind::ResourceBusy
    );
    assert!(manager.can_connect("default"));
    drop(player);
    manager.remove("default").await.unwrap();
    assert!(manager.snapshot().is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn cancelled_removal_finishes_and_rejects_all_operations_while_retiring() {
    let mut fixture = Fixture::new(0, "stubborn");
    fixture.definition().stop_timeout = Duration::from_millis(400);
    let manager = fixture.manager();
    manager.start("default").await.unwrap();
    let removal_manager = manager.clone();
    let removal = tokio::spawn(async move { removal_manager.remove("default").await });
    let deadline = Instant::now() + Duration::from_secs(2);
    while manager.can_connect("default") {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    removal.abort();
    assert!(manager.reserve("default").is_err());
    assert!(manager.request_start("default").is_err());
    assert!(manager.request_stop("default").is_err());
    assert_eq!(
        manager.remove("default").await.unwrap_err().kind(),
        std::io::ErrorKind::ResourceBusy
    );
    assert_eq!(
        manager.add(&fixture.config, "default").unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    wait_removed(&manager).await;
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    assert!(!manager.can_connect("default"));
    manager.shutdown().await;
}

#[tokio::test]
async fn removal_interrupts_startup_without_abandoning_process() {
    let fixture = Fixture::new(10_000, "normal");
    let manager = empty_manager(&fixture);
    manager.add(&fixture.config, "default").unwrap();
    manager.request_start("default").unwrap();
    wait_file(&fixture.directory.join("starts")).await;
    let before = Instant::now();
    manager.remove("default").await.unwrap();
    assert!(before.elapsed() < Duration::from_secs(1));
    assert!(manager.snapshot().is_empty());
    assert_eq!(fixture.starts(), 1);
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    manager.shutdown().await;
}

#[tokio::test]
async fn shutdown_waits_for_retiring_children_and_prevents_dynamic_additions() {
    let mut fixture = Fixture::new(0, "stubborn");
    fixture.definition().stop_timeout = Duration::from_millis(300);
    let manager = fixture.manager();
    manager.start("default").await.unwrap();
    let removal_manager = manager.clone();
    let removal = tokio::spawn(async move { removal_manager.remove("default").await });
    while manager.can_connect("default") {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    manager.shutdown().await;
    removal.await.unwrap().unwrap();
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    assert!(manager.add(&fixture.config, "default").is_err());
    assert!(manager.remove("default").await.is_err());
    manager.shutdown().await;
}

#[tokio::test]
async fn dynamic_add_requires_launch_and_valid_managed_definition() {
    let fixture = Fixture::new(0, "normal");
    let manager = ManagedServers::new(&fixture.config, fixture.players.clone());
    assert!(manager.add(&fixture.config, "default").is_err());
    assert_eq!(fixture.starts(), 0);
    let manager = empty_manager(&fixture);
    assert_eq!(
        manager.add(&fixture.config, "missing").unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    let mut invalid = fixture.config.clone();
    invalid.backends.clear();
    assert_eq!(
        manager.add(&invalid, "default").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    invalid = fixture.config.clone();
    invalid
        .managed_servers
        .get_mut("default")
        .unwrap()
        .command
        .clear();
    assert_eq!(
        manager.add(&invalid, "default").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(manager.snapshot().is_empty());
    assert_eq!(fixture.starts(), 0);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_dynamic_additions_create_exactly_one_worker() {
    let mut fixture = Fixture::new(0, "normal");
    fixture.definition().autostart = true;
    let manager = empty_manager(&fixture);
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let mut additions = Vec::new();
    for _ in 0..8 {
        let manager = manager.clone();
        let config = fixture.config.clone();
        let barrier = barrier.clone();
        additions.push(tokio::spawn(async move {
            barrier.wait().await;
            manager.add(&config, "default")
        }));
    }
    barrier.wait().await;
    let mut added = 0;
    for addition in additions {
        match addition.await.unwrap() {
            Ok(()) => added += 1,
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists),
        }
    }
    assert_eq!(added, 1);
    wait_state(&manager, "running").await;
    assert_eq!(fixture.starts(), 1);
    manager.remove("default").await.unwrap();
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn racing_dynamic_add_and_shutdown_cannot_leave_a_live_child() {
    let mut fixture = Fixture::new(100, "normal");
    fixture.definition().autostart = true;
    let manager = empty_manager(&fixture);
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let adding_manager = manager.clone();
    let adding_barrier = barrier.clone();
    let config = fixture.config.clone();
    let adding = tokio::spawn(async move {
        adding_barrier.wait().await;
        adding_manager.add(&config, "default")
    });
    let shutting_manager = manager.clone();
    let shutting_barrier = barrier.clone();
    let shutting = tokio::spawn(async move {
        shutting_barrier.wait().await;
        shutting_manager.shutdown().await;
    });
    barrier.wait().await;
    let added = adding.await.unwrap();
    shutting.await.unwrap();
    if let Err(error) = added {
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
    }
    assert!(manager.snapshot().iter().all(|server| server.pid.is_none()));
    assert!(std::net::TcpStream::connect(fixture.address).is_err());
    assert!(manager.add(&fixture.config, "default").is_err());
}
