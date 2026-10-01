//! Black-box regressions for the executable's startup contract.
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

fn run(args: &[&str]) -> Output {
    run_in(args, None)
}

fn run_in(args: &[&str], directory: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rift"));
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    let mut child = command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("rift did not exit for {args:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn help_is_successful_and_describes_the_interface() {
    for flag in ["--help", "-h"] {
        let output = run(&[flag]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("<listen-ip:port> <backend-ip:port>"));
        assert!(text.contains("25565"));
        assert!(text.contains("--route"));
        assert!(text.contains("--default"));
        assert!(text.contains("--version"));
        assert!(text.contains("--license"));
        assert!(text.contains("rift init"));
        assert!(text.contains("rift check"));
        assert!(text.contains("rift --check [path]"));
        assert!(text.contains("No arguments: load ./rift.lua if present"));
        assert!(text.contains("rift admin"));
        assert!(text.contains("existing sessions continue"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn version_matches_the_package_and_exits_successfully() {
    for flag in ["--version", "-V"] {
        let output = run(&[flag]);
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            concat!("rift ", env!("CARGO_PKG_VERSION"))
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn malformed_arguments_and_proxy_loops_are_rejected() {
    for args in [
        vec!["127.0.0.1:25565"],
        vec!["one", "two", "three"],
        vec!["localhost:25565", "127.0.0.1:25566"],
        vec!["127.0.0.1:25565", "bad-backend"],
        vec!["127.0.0.1:25565", "127.0.0.1:25565"],
        vec!["0.0.0.0:25565", "127.0.0.1:25565"],
        vec!["[::1]:25565", "[::1]:25565"],
        vec!["[::]:25565", "[::1]:25565"],
        vec!["127.0.0.1:25565", "--route"],
        vec!["127.0.0.1:25565", "--route", "example.com"],
        vec!["127.0.0.1:25565", "--route", "example.com=127.0.0.1:25565"],
        vec![
            "127.0.0.1:25565",
            "--route",
            "*.example.com=127.0.0.1:25565",
        ],
        vec!["127.0.0.1:25565", "--default", "127.0.0.1:25565"],
        vec![
            "127.0.0.1:25565",
            "--route",
            "*=host:1",
            "--default",
            "other:2",
        ],
        vec!["127.0.0.1:25565", "--unknown", "host:1"],
        vec!["127.0.0.1:25565", "host:1", "--route", "example.com=host:2"],
    ] {
        let output = run(&args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn occupied_listener_fails_instead_of_hanging() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let output = run(&[&occupied.local_addr().unwrap().to_string(), "127.0.0.1:0"]);
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        run_in(args, Some(&self.0))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn init_creates_a_valid_documented_config_and_never_overwrites() {
    let fixture = Fixture::new();
    for (init_args, check_args, filename) in [
        (vec!["init"], vec!["check"], "rift.lua"),
        (
            vec!["init", "custom.lua"],
            vec!["check", "custom.lua"],
            "custom.lua",
        ),
    ] {
        let output = fixture.run(&init_args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let source = fs::read_to_string(fixture.0.join(filename)).unwrap();
        assert_eq!(source, include_str!("../examples/rift.lua"));
        assert!(source.contains("existing sessions continue"));
        let checked = fixture.run(&check_args);
        assert!(
            checked.status.success(),
            "{}",
            String::from_utf8_lossy(&checked.stderr)
        );
        assert!(String::from_utf8_lossy(&checked.stdout).contains("configuration valid"));

        fs::write(fixture.0.join(filename), "keep my configuration").unwrap();
        let output = fixture.run(&init_args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
        assert_eq!(
            fs::read_to_string(fixture.0.join(filename)).unwrap(),
            "keep my configuration"
        );
    }
}

#[test]
fn check_validates_without_binding_or_resolving_admin_secrets() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = format!("return {{
        listeners = {{ public = '{}' }},
        backends = {{ lobby = '127.0.0.1:0' }},
        routes = {{ public = 'lobby' }},
        admin = {{ listen = '127.0.0.1:0', token_env = 'RIFT_TEST_UNSET_SECRET', permissions = {{ 'status' }} }}
    }}", occupied.local_addr().unwrap());
    fs::write(fixture.0.join("rift.lua"), &source).unwrap();
    for args in [
        vec!["check"],
        vec!["--check"],
        vec!["check", "rift.lua"],
        vec!["--check", "rift.lua"],
    ] {
        let output = fixture.run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
    }
    fs::write(
        fixture.0.join("rift.lua"),
        source.replace("'lobby'", "'missing'"),
    )
    .unwrap();
    for args in [vec!["check"], vec!["--check"]] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("rift.lua") && error.contains("unknown backend"),
            "{error}"
        );
    }
}

#[test]
fn setup_commands_report_paths_and_reject_extra_arguments() {
    let fixture = Fixture::new();
    for args in [
        vec!["check"],
        vec!["--check"],
        vec!["check", "missing.lua"],
        vec!["--check", "missing.lua"],
        vec!["init", "missing/parent.lua"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(args.get(1).copied().unwrap_or("rift.lua"))
        );
    }
    for args in [
        vec!["init", "one", "two"],
        vec!["check", "one", "two"],
        vec!["--check", "one", "two"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Usage:"));
    }
    assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
}
