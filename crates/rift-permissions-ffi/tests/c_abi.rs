//! Compiles `tests/c/smoke.c` against the header and the cdylib and runs it.
//! Skipped when no C compiler (`cc`, or `$CC`) is available.

use std::{env, fs, path::PathBuf, process::Command};

/// The cdylib cargo built next to this test binary.
fn library() -> PathBuf {
    let deps = env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let name = format!(
        "{}rift_perms{}",
        env::consts::DLL_PREFIX,
        env::consts::DLL_SUFFIX
    );
    [deps.join(&name), deps.parent().unwrap().join(&name)]
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| panic!("{name} not found near {}", deps.display()))
}

#[test]
fn a_c_program_uses_the_library() {
    let compiler = env::var("CC").unwrap_or_else(|_| "cc".into());
    if Command::new(&compiler).arg("--version").output().is_err() {
        eprintln!("skipping: no C compiler ({compiler})");
        return;
    }
    if cfg!(windows) {
        eprintln!("skipping: the test links with Unix compiler flags");
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = env::temp_dir().join(format!("rift-permissions-c-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let library = library();
    let program = dir.join("smoke");
    let status = Command::new(&compiler)
        .args(["-std=c99", "-Wall", "-Werror", "-o"])
        .arg(&program)
        .arg(root.join("tests/c/smoke.c"))
        .arg("-I")
        .arg(root.join("include"))
        .arg(&library)
        .arg(format!(
            "-Wl,-rpath,{}",
            library.parent().unwrap().display()
        ))
        .status()
        .unwrap();
    assert!(status.success(), "compiling smoke.c failed");

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    fs::write(dir.join("ca.pem"), cert.cert.pem()).unwrap();
    fs::write(
        dir.join("fallback.json"),
        r#"{"groups": [{"name": "default", "nodes": [
            {"kind": "permission", "key": "spawn", "value": true},
            {"kind": "permission", "key": "essentials.fly", "value": false},
            {"kind": "prefix", "priority": 1, "text": "[Guest] "}]}]}"#,
    )
    .unwrap();
    let config = serde_json::json!({
        "address": "127.0.0.1:9", "server_name": "localhost", "ca": dir.join("ca.pem"),
        "token_env": "RIFT_PERMS_C_TOKEN", "server": "lobby",
        "offline_policy": {"mode": "fallback_file", "path": dir.join("fallback.json")},
    });
    let output = Command::new(&program)
        .arg(config.to_string())
        .env("RIFT_PERMS_C_TOKEN", "unused")
        .output()
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "smoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "ok\n");
}
