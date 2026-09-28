//! Black-box regressions for the executable's startup contract.
use std::{
    net::TcpListener,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn run(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
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
