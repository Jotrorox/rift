//! The C ABI called from Rust: argument checking, the offline path, and a
//! round trip through a real Rift broker with the test playing the proxy.

use std::{
    collections::BTreeSet,
    ffi::{CStr, CString, c_char, c_void},
    fs,
    path::PathBuf,
    ptr,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use ::rift_perms::*;
use bytes::Bytes;
use rift::messaging::{
    Broker,
    quic::{AuthConfig, Identity, Server, server_config_from_der},
};
use rift_permissions::{
    CompiledSubject, Uuid,
    wire::{self, Applied, GroupsReply, HelloReply},
};
use rustls::pki_types::PrivateKeyDer;
use serde_json::{Value, json};

/// A player's UUID as the 16 bytes C passes.
const ALICE: [u8; 16] = 0xa11ce_u128.to_be_bytes();

/// A scratch directory with a CA certificate and a fallback file.
struct Dir {
    path: PathBuf,
    cert: rcgen::CertifiedKey<rcgen::KeyPair>,
}

impl Dir {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-permissions-ffi-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        fs::write(path.join("ca.pem"), cert.cert.pem()).unwrap();
        fs::write(
            path.join("fallback.json"),
            r#"{"groups": [{"name": "default", "nodes": [
                {"kind": "permission", "key": "spawn", "value": true},
                {"kind": "permission", "key": "essentials.fly", "value": false, "contexts": {"server": "lobby"}},
                {"kind": "prefix", "priority": 1, "text": "[Guest] "}]}]}"#,
        )
        .unwrap();
        Self { path, cert }
    }

    /// A configuration whose token is in `CARGO_PKG_NAME`, which cargo sets
    /// for every test run.
    fn config(&self, address: &str) -> CString {
        let config = json!({
            "address": address, "server_name": "localhost",
            "ca": self.path.join("ca.pem"), "token_env": "CARGO_PKG_NAME", "server": "lobby",
            "join_timeout_ms": 1000, "request_timeout_ms": 1000,
            "offline_policy": {"mode": "fallback_file", "path": self.path.join("fallback.json")},
        });
        CString::new(config.to_string()).unwrap()
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn c(text: &str) -> CString {
    CString::new(text).unwrap()
}

/// Take ownership of a returned string.
fn take(text: *mut c_char) -> String {
    assert!(!text.is_null());
    let owned = unsafe { CStr::from_ptr(text) }.to_str().unwrap().to_owned();
    unsafe { rift_perms_free(text) };
    owned
}

fn json_of(text: *mut c_char) -> Value {
    serde_json::from_str(&take(text)).unwrap()
}

static CALLS: AtomicUsize = AtomicUsize::new(0);
static LAST_REVISION: AtomicU64 = AtomicU64::new(u64::MAX);

unsafe extern "C" fn on_snapshot(uuid: *const u8, revision: u64, user: *mut c_void) {
    assert_eq!(unsafe { std::slice::from_raw_parts(uuid, 16) }, ALICE);
    assert_eq!(user as usize, 7);
    CALLS.fetch_add(1, Ordering::SeqCst);
    LAST_REVISION.store(revision, Ordering::SeqCst);
}

#[test]
fn null_and_invalid_arguments_are_refused() {
    unsafe {
        let mut err = ptr::null_mut();
        assert!(rift_perms_connect(ptr::null(), &mut err).is_null());
        assert!(take(err).contains("config_json"));
        assert!(rift_perms_connect(c("{not json").as_ptr(), &mut err).is_null());
        assert!(take(err).contains("config"));
        assert!(rift_perms_connect(c("{}").as_ptr(), ptr::null_mut()).is_null());
        let node = c("fly");
        assert_eq!(
            rift_perms_check(ptr::null_mut(), ALICE.as_ptr(), node.as_ptr(), ptr::null()),
            -2
        );
        assert!(rift_perms_meta(ptr::null_mut(), ALICE.as_ptr(), ptr::null()).is_null());
        assert_eq!(rift_perms_revision(ptr::null_mut()), 0);
        assert_eq!(rift_perms_join(ptr::null_mut(), ALICE.as_ptr()), -2);
        let mut reply = ptr::null_mut();
        assert_eq!(
            rift_perms_apply(ptr::null_mut(), ptr::null(), ptr::null(), &mut reply),
            -2
        );
        assert_eq!(json_of(reply)["code"], "invalid_request");
        rift_perms_leave(ptr::null_mut(), ptr::null());
        rift_perms_subscribe(ptr::null_mut(), None, ptr::null_mut());
        rift_perms_close(ptr::null_mut());
        rift_perms_free(ptr::null_mut());
    }
}

#[test]
fn an_unreachable_proxy_serves_the_fallback_file() {
    let dir = Dir::new();
    unsafe {
        let mut err = ptr::null_mut();
        let perms = rift_perms_connect(dir.config("127.0.0.1:9").as_ptr(), &mut err);
        assert!(!perms.is_null() && err.is_null());
        let (spawn, fly) = (c("spawn"), c("essentials.fly"));
        assert_eq!(
            rift_perms_check(perms, ALICE.as_ptr(), spawn.as_ptr(), ptr::null()),
            -2
        );
        assert_eq!(rift_perms_join(perms, ALICE.as_ptr()), 0);
        assert_eq!(
            rift_perms_check(perms, ALICE.as_ptr(), spawn.as_ptr(), ptr::null()),
            1
        );
        // The handle's server `lobby` is filled in, matching the deny.
        assert_eq!(
            rift_perms_check(perms, ALICE.as_ptr(), fly.as_ptr(), c("").as_ptr()),
            0
        );
        let elsewhere = c(r#"{"server": "survival"}"#);
        assert_eq!(
            rift_perms_check(perms, ALICE.as_ptr(), fly.as_ptr(), elsewhere.as_ptr()),
            -1
        );
        assert_eq!(
            rift_perms_check(perms, ALICE.as_ptr(), fly.as_ptr(), c("{bad").as_ptr()),
            -2
        );
        let meta = json_of(rift_perms_meta(perms, ALICE.as_ptr(), ptr::null()));
        assert_eq!(meta["prefix"], "[Guest] ");
        assert_eq!(meta["primary_group"], "default");

        let mut reply = ptr::null_mut();
        let change = c(r#"{"op": "delete_track", "name": "ranks"}"#);
        assert_eq!(
            rift_perms_apply(perms, ptr::null(), change.as_ptr(), &mut reply),
            -1
        );
        assert_eq!(json_of(reply)["code"], "unavailable");
        let invalid = c(r#"{"op": "delete_group", "name": "default"}"#);
        assert_eq!(
            rift_perms_apply(perms, ptr::null(), invalid.as_ptr(), &mut reply),
            -1
        );
        assert_eq!(json_of(reply)["code"], "invalid_request");
        let actor = c("not-a-uuid");
        assert_eq!(
            rift_perms_apply(perms, actor.as_ptr(), change.as_ptr(), &mut reply),
            -2
        );
        assert_eq!(json_of(reply)["code"], "invalid_request");
        assert_eq!(
            rift_perms_apply(perms, ptr::null(), c("{}").as_ptr(), ptr::null_mut()),
            -2
        );

        rift_perms_leave(perms, ALICE.as_ptr());
        assert_eq!(rift_perms_revision(perms), 0);
        rift_perms_close(perms);
    }
}

#[test]
fn applies_joins_and_notifies_through_a_proxy() {
    let dir = Dir::new();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let broker = Broker::default();
    let server = runtime.block_on(async {
        let tls = server_config_from_der(
            vec![dir.cert.cert.der().clone()],
            PrivateKeyDer::Pkcs8(dir.cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let identity = Identity {
            name: "server.lobby".into(),
            token: env!("CARGO_PKG_NAME").into(),
            publish: vec!["rift.perms.>".into()],
            subscribe: vec![
                "rift.perms.update".into(),
                "rift.perms.update.>".into(),
                "rift.events.permissions".into(),
            ],
            control: false,
        };
        let auth = AuthConfig {
            identities: vec![identity],
            ..AuthConfig::default()
        };
        let server =
            Server::bind("127.0.0.1:0".parse().unwrap(), tls, broker.clone(), auth).unwrap();
        tokio::spawn(respond(broker.clone()));
        server
    });
    let address = server.local_addr().unwrap().to_string();
    unsafe {
        let perms = rift_perms_connect(dir.config(&address).as_ptr(), ptr::null_mut());
        assert!(!perms.is_null());
        rift_perms_subscribe(perms, Some(on_snapshot), 7 as *mut c_void);
        let deadline = Instant::now() + Duration::from_secs(5);
        while rift_perms_revision(perms) != 3 {
            assert!(Instant::now() < deadline, "no hello");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rift_perms_join(perms, ALICE.as_ptr()), 1);
        assert_eq!(
            rift_perms_check(perms, ALICE.as_ptr(), c("fly").as_ptr(), ptr::null()),
            1
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(LAST_REVISION.load(Ordering::SeqCst), 3);

        let mut reply = ptr::null_mut();
        let actor = c(&Uuid::from_bytes(ALICE).to_string());
        let change = c(
            r#"{"op": "set_primary_group", "user": "00000000000000000000000000000001", "group": "vip"}"#,
        );
        assert_eq!(
            rift_perms_apply(perms, actor.as_ptr(), change.as_ptr(), &mut reply),
            0
        );
        let applied: Applied = serde_json::from_str(&take(reply)).unwrap();
        assert_eq!(applied.revision, 4);
        rift_perms_close(perms);
    }
    drop(server);
}

/// The proxy's handlers: revision 3, every player may fly.
async fn respond(broker: Broker) {
    let mut requests = broker.subscribe("rift.perms.>", None).unwrap();
    while let Ok(message) = requests.recv().await {
        let Some(inbox) = message.reply else {
            continue;
        };
        let data = match &*message.subject {
            wire::HELLO => serde_json::to_value(HelloReply {
                revision: 3,
                protocol: wire::PROTOCOL_VERSION,
                now: rift_permissions::unix_now(),
                limits: Default::default(),
                external_contexts: vec![],
            }),
            wire::SUBJECT_GET => {
                let request: Value = serde_json::from_slice(&message.payload).unwrap();
                let subject: CompiledSubject = serde_json::from_value(json!({
                    "revision": 3, "uuid": request["uuid"], "primary_group": "default",
                    "entries": [{"k": "perm", "key": "fly", "v": true, "rank": 0}]}))
                .unwrap();
                serde_json::to_value(subject)
            }
            wire::GROUPS_GET => serde_json::to_value(GroupsReply {
                revision: 3,
                groups: vec![],
            }),
            wire::APPLY => serde_json::to_value(Applied {
                revision: 4,
                ops: 1,
                changed_users: vec![],
                changed_groups: vec![],
            }),
            _ => continue,
        };
        let reply = json!({"ok": true, "data": data.unwrap()});
        let _ = broker.publish(&inbox, Bytes::from(reply.to_string()));
    }
}

/// `include/rift_perms.h` declares exactly the functions `src/lib.rs`
/// exports. Regenerate it with `cbindgen --config cbindgen.toml --output
/// include/rift_perms.h` when this fails.
#[test]
fn header_matches_the_exported_functions() {
    let root = env!("CARGO_MANIFEST_DIR");
    let header = fs::read_to_string(format!("{root}/include/rift_perms.h")).unwrap();
    let source = fs::read_to_string(format!("{root}/src/lib.rs")).unwrap();
    // Declarations start in the first column; comments and directives do not.
    let declared: BTreeSet<&str> = header
        .lines()
        .filter(|line| !line.starts_with([' ', '/', '#', '*']))
        .filter_map(|line| line.split_once('('))
        .filter_map(|(head, _)| head.rsplit([' ', '*']).next())
        .collect();
    let exported: BTreeSet<&str> = source
        .split("extern \"C\" fn ")
        .skip(1)
        .map(|rest| {
            rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()
                .unwrap()
        })
        .filter(|name| !name.is_empty())
        .collect();
    assert_eq!(exported.len(), 10, "{exported:?}");
    assert_eq!(declared, exported);
}
