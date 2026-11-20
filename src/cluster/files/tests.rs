//! Configuration and files on simulated peers (plan step 8), each with its
//! own directory: `rift.lua`, `templates/` and `cache/blobs/`. The
//! configuration service of each peer is a stand-in reloader that reads
//! the peer's `rift.lua` and reports it as running, as the binary's does
//! after a reload.
use super::*;
use crate::cluster::sim::{Sim, until};
use std::sync::atomic::{AtomicUsize, Ordering};

const WAIT: Duration = Duration::from_secs(60);

const BASE: &str = "rift.config.listeners.public = '127.0.0.1:25565'
rift.config.backends.lobby = '127.0.0.1:25566'
rift.config.backends.other = '127.0.0.1:25567'
rift.config.routes.public = 'lobby'
";

/// A valid configuration that differs from others only in a comment.
fn source(marker: &str) -> String {
    format!("{BASE}-- {marker}\n")
}

fn segments(path: &str) -> Vec<String> {
    path.split('/').map(str::to_owned).collect()
}

fn reloader(state: &Arc<FilesState>, path: PathBuf, reloads: Arc<AtomicUsize>) -> Reloader {
    let state = Arc::downgrade(state);
    Arc::new(move || -> BoxFuture<'static, Result<(), String>> {
        let state = state.clone();
        let path = path.clone();
        let reloads = reloads.clone();
        Box::pin(async move {
            reloads.fetch_add(1, Ordering::SeqCst);
            let source = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(state) = state.upgrade() {
                state.report_running(&source);
            }
            Ok(())
        })
    })
}

type States = Arc<Mutex<HashMap<usize, Arc<FilesState>>>>;

struct Net {
    sim: Sim,
    states: States,
    attached: Vec<Option<Attached>>,
    reloads: Vec<Arc<AtomicUsize>>,
}

impl Net {
    /// `count` peers with the files component, not started.
    fn new(count: usize) -> Self {
        let mut sim = Sim::new(count);
        let states: States = Arc::default();
        let directories: Vec<PathBuf> = (0..count).map(|index| sim.directory(index)).collect();
        let created = states.clone();
        sim.components = Some(Arc::new(move |index| {
            let state = FilesState::new(&directories[index]);
            created.lock().unwrap().insert(index, state.clone());
            components(&state)
        }));
        Self {
            sim,
            states,
            attached: (0..count).map(|_| None).collect(),
            reloads: (0..count).map(|_| Arc::default()).collect(),
        }
    }

    /// `count` voters, formed, with `config` and `manifest` entries enabled.
    async fn form(count: usize) -> Self {
        let mut net = Self::new(count);
        net.sim.form(count).await;
        for index in 0..count {
            net.enabled(index).await;
        }
        net
    }

    async fn enabled(&self, index: usize) {
        let peer = self.sim.peer(index).clone();
        until(WAIT, move || {
            peer.is_enabled(EntryKind::Config).unwrap_or(false)
                && peer.is_enabled(EntryKind::Manifest).unwrap_or(false)
        })
        .await;
    }

    fn state(&self, index: usize) -> Arc<FilesState> {
        self.states.lock().unwrap()[&index].clone()
    }

    fn path(&self, index: usize) -> PathBuf {
        self.sim.directory(index).join("rift.lua")
    }

    fn storage(&self, index: usize) -> PathBuf {
        self.sim.directory(index).join("templates")
    }

    /// Connect peer `index` without its tasks: no deployer, no
    /// materialization, no reports.
    fn connect(&self, index: usize) {
        self.state(index).connect(self.sim.peer(index));
    }

    async fn attach(&mut self, index: usize) {
        let state = self.state(index);
        let options = AttachOptions {
            config_path: Some(self.path(index)),
            template_storage: Some(self.storage(index)),
            software_cache: Some(self.sim.directory(index).join("software")),
            template_sources: BTreeMap::new(),
        };
        let attached = attach(self.sim.peer(index), &state, options).await.unwrap();
        state.set_reloader(reloader(
            &state,
            self.path(index),
            self.reloads[index].clone(),
        ));
        self.attached[index] = Some(attached);
    }

    /// Every peer runs `text` from its `rift.lua`, attached.
    async fn start_with(&mut self, text: &str) {
        for index in 0..self.attached.len() {
            fs::write(self.path(index), text).unwrap();
            self.attach(index).await;
            self.state(index).report_running(text);
        }
    }

    async fn shutdown(mut self) {
        for attached in &mut self.attached {
            drop(attached.take());
        }
        self.sim.shutdown().await;
    }
}

async fn file_becomes(path: PathBuf, expected: String) {
    until(WAIT, move || {
        fs::read_to_string(&path).is_ok_and(|text| text == expected)
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_deployment_on_a_follower_reaches_every_peer_and_its_history() {
    let mut net = Net::form(3).await;
    net.start_with(&source("a")).await;
    let leader = net.sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let id = net
        .state(follower)
        .deploy(&source("b"), &net.path(follower), "tester", "save")
        .await
        .unwrap();
    // The proposer's configuration transaction applies it itself.
    net.state(follower).report_running(&source("b"));
    for index in 0..3 {
        file_becomes(net.path(index), source("b")).await;
        let state = net.state(index);
        until(WAIT, move || {
            state.deployments_value().is_ok_and(|history| {
                history["deployments"][0]["id"] == id
                    && history["deployments"][0]["actor"] == "tester"
                    && history["deployments"][0]["action"] == "save"
            })
        })
        .await;
    }
    // Every other peer reloaded through its configuration service, and every
    // peer reports that it runs the deployment.
    for index in (0..3).filter(|index| *index != follower) {
        assert!(net.reloads[index].load(Ordering::SeqCst) >= 1, "p{index}");
    }
    assert_eq!(net.reloads[follower].load(Ordering::SeqCst), 0);
    let observer = net.sim.peer(leader).clone();
    until(WAIT, move || {
        let status = status(&observer);
        status["peers"].as_array().is_some_and(|peers| {
            peers.len() == 3 && peers.iter().all(|peer| peer["in_sync"] == true)
        })
    })
    .await;
    let status = status(net.sim.peer(leader));
    assert_eq!(status["deployment"]["id"], id);
    assert!(
        net.sim
            .peer(leader)
            .render_metrics()
            .contains("rift_cluster_config_in_sync{peer=\"p0\"} 1")
    );
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_rollback_deploys_an_earlier_source_everywhere() {
    let mut net = Net::form(3).await;
    net.start_with(&source("a")).await;
    let first = net
        .state(0)
        .deploy(&source("a"), &net.path(0), "tester", "save")
        .await
        .unwrap();
    net.state(0)
        .deploy(&source("b"), &net.path(0), "tester", "save")
        .await
        .unwrap();
    net.state(0).report_running(&source("b"));
    for index in 0..3 {
        file_becomes(net.path(index), source("b")).await;
    }
    let earlier = net.state(2).deployment_source(first).unwrap().unwrap();
    assert_eq!(earlier, source("a"));
    net.state(2)
        .deploy(&earlier, &net.path(2), "operator", "rollback")
        .await
        .unwrap();
    net.state(2).report_running(&earlier);
    for index in 0..3 {
        file_becomes(net.path(index), source("a")).await;
        let state = net.state(index);
        until(WAIT, move || {
            state.deployments_value().is_ok_and(|history| {
                history["deployments"].as_array().map(Vec::len) == Some(3)
                    && history["deployments"][0]["action"] == "rollback"
            })
        })
        .await;
    }
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn audit_records_are_replicated_once() {
    let mut net = Net::form(3).await;
    net.start_with(&source("a")).await;
    let record = json!({"timestamp_unix_ms": 1, "actor": "tester", "action": "POST console", "target": "lobby-1", "outcome": "accepted", "status": 200});
    net.state(1).audit(record.clone()).await.unwrap();
    for index in 0..3 {
        let state = net.state(index);
        until(WAIT, move || {
            state
                .audit_records()
                .is_ok_and(|records| !records.is_empty())
        })
        .await;
        assert_eq!(
            net.state(index).audit_records().unwrap(),
            std::slice::from_ref(&record)
        );
    }
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_starting_peer_adopts_deployments_and_refuses_hand_edits() {
    let mut net = Net::form(3).await;
    // p2 stays detached: only the startup rule touches its file.
    for index in 0..2 {
        fs::write(net.path(index), source("a")).unwrap();
        net.attach(index).await;
    }
    net.connect(2);
    let cluster = net.sim.peer(2).clone();
    let path = net.path(2);
    assert_eq!(
        startup(&cluster, &path, false).await.unwrap(),
        Startup::Undeployed
    );
    let older = net
        .state(0)
        .deploy(&source("a"), &net.path(0), "tester", "save")
        .await
        .unwrap();
    let latest = net
        .state(0)
        .deploy(&source("b"), &net.path(0), "tester", "save")
        .await
        .unwrap();
    cluster.read_barrier().await.unwrap();
    // The last deployment starts as it is.
    fs::write(&path, source("b")).unwrap();
    assert_eq!(
        startup(&cluster, &path, false).await.unwrap(),
        Startup::Current { id: latest }
    );
    // An older deployed revision is replaced by the last one.
    fs::write(&path, source("a")).unwrap();
    let outcome = startup(&cluster, &path, false).await.unwrap();
    assert_eq!(
        outcome,
        Startup::Adopted {
            id: latest,
            from: Some(older)
        }
    );
    assert!(outcome.rewrote());
    assert_eq!(fs::read_to_string(&path).unwrap(), source("b"));
    // A hand edit is refused and kept.
    let edited = format!("{BASE}-- edited by hand\n");
    fs::write(&path, &edited).unwrap();
    let error = startup(&cluster, &path, false).await.unwrap_err();
    assert!(error.to_string().contains("--adopt-deployed"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), edited);
    // Unless the operator says to adopt; the edit is kept beside.
    assert_eq!(
        startup(&cluster, &path, true).await.unwrap(),
        Startup::Adopted {
            id: latest,
            from: None
        }
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), source("b"));
    assert_eq!(
        fs::read_to_string(path.with_file_name("rift.lua.hand-edited")).unwrap(),
        edited
    );
    // A fresh peer without a configuration receives the deployed one.
    fs::remove_file(&path).unwrap();
    assert_eq!(
        startup(&cluster, &path, false).await.unwrap(),
        Startup::Received { id: latest }
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), source("b"));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_shared_section_that_depends_on_the_peer_blocks_the_deployment() {
    let net = Net::form(3).await;
    for index in 0..3 {
        net.connect(index);
    }
    let path = net.path(0);
    let shared_branch =
        format!("{BASE}if rift.peer == 'p2' then rift.config.backends.extra = '127.0.0.1:1' end\n");
    let error = net
        .state(0)
        .deploy(&shared_branch, &path, "tester", "save")
        .await
        .unwrap_err();
    assert!(
        error.contains("p2") && error.contains("backends"),
        "{error}"
    );
    assert!(net.state(0).latest_deployment().unwrap().is_none());
    // Peer sections may differ.
    let peer_branch =
        format!("{BASE}if rift.peer == 'p2' then rift.config.routes.public = 'other' end\n");
    let id = net
        .state(0)
        .deploy(&peer_branch, &path, "tester", "save")
        .await
        .unwrap();
    let deployed = net.state(0).latest_deployment().unwrap().unwrap();
    assert_eq!(deployed.id, id);
    // The state machine itself refuses a deployment whose members disagree
    // or that was not evaluated for every member.
    let members = |hashes: &[(&str, &str)]| -> BTreeMap<String, String> {
        hashes
            .iter()
            .map(|(peer, hash)| ((*peer).to_owned(), (*hash).to_owned()))
            .collect()
    };
    let deploy = |members: BTreeMap<String, String>| {
        ConfigEntry::Deploy {
            actor: "tester".into(),
            action: "save".into(),
            revision: revision(&peer_branch),
            source: peer_branch.clone(),
            shared: Shared {
                hash: "x".into(),
                sections: BTreeMap::new(),
            },
            members,
        }
        .entry()
    };
    let reply = net
        .sim
        .peer(1)
        .propose(deploy(members(&[("p0", "x"), ("p1", "x"), ("p2", "y")])))
        .await
        .unwrap();
    assert!(
        matches!(&reply, Reply::Err(reason) if reason.contains("p2")),
        "{reply:?}"
    );
    let reply = net
        .sim
        .peer(1)
        .propose(deploy(members(&[("p0", "x"), ("p2", "x")])))
        .await
        .unwrap();
    assert!(
        matches!(&reply, Reply::Err(reason) if reason.contains(config::NOT_VALIDATED) && reason.contains("p1")),
        "{reply:?}"
    );
    assert_eq!(net.state(0).latest_deployment().unwrap().unwrap().id, id);
    // A peer that runs the deployment but evaluates a shared section
    // differently is named with the section.
    let mut sections = deployed.shared.sections.clone();
    sections.insert("backends".into(), "0".repeat(64));
    let report = ConfigEntry::Report {
        peer: "p1".into(),
        digest: deployed.digest.clone(),
        shared: Shared {
            hash: "1".repeat(64),
            sections,
        },
        error: None,
    };
    net.sim.peer(1).propose(report.entry()).await.unwrap();
    let status = status(net.sim.peer(1));
    let p1 = status["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["peer"] == "p1")
        .unwrap()
        .clone();
    assert_eq!(p1["in_sync"], true);
    assert_eq!(p1["shared_differs"], json!(["backends"]));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn blobs_are_fetched_by_hash_and_corrupt_copies_refused() {
    let mut net = Net::form(3).await;
    net.start_with(&source("a")).await;
    let blob = net
        .state(0)
        .blobs()
        .store_bytes(b"template contents")
        .unwrap();
    blobs::fetch(net.sim.peer(1), net.state(1).blobs(), &blob, Some("p0"))
        .await
        .unwrap();
    assert_eq!(
        fs::read(net.state(1).blobs().path(&blob.sha256)).unwrap(),
        b"template contents"
    );
    let other = net.state(0).blobs().store_bytes(b"original bytes").unwrap();
    // Same length, other content: only the hash shows it.
    fs::write(net.state(0).blobs().path(&other.sha256), b"corrupt bytes!").unwrap();
    let error = blobs::fetch(net.sim.peer(2), net.state(2).blobs(), &other, Some("p0"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("corrupt"), "{error}");
    assert!(!net.state(2).blobs().has(&other.sha256));
    // A good copy elsewhere is used once p0's is refused.
    net.state(1).blobs().store_bytes(b"original bytes").unwrap();
    blobs::fetch(net.sim.peer(2), net.state(2).blobs(), &other, Some("p0"))
        .await
        .unwrap();
    assert_eq!(
        fs::read(net.state(2).blobs().path(&other.sha256)).unwrap(),
        b"original bytes"
    );
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_manifest_commits_only_after_a_majority_holds_its_blobs() {
    let mut net = Net::form(3).await;
    net.start_with(&source("a")).await;
    let hash = shared::sha256(b"hello");
    // The other voters refuse the blobs: nothing is proposed.
    for index in [1, 2] {
        net.state(index).refuse_blobs.store(true, Ordering::SeqCst);
    }
    let error = net
        .state(0)
        .put_template(&segments("lobby/a.txt"), false, b"hello")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("voters"), "{error}");
    for index in 0..3 {
        let folders = net.sim.peer(index).state().read(manifest::folders).unwrap();
        assert!(folders.is_empty(), "p{index}: {folders:?}");
    }
    for index in [1, 2] {
        net.state(index).refuse_blobs.store(false, Ordering::SeqCst);
    }
    // With p2 cut off, p0 and p1 are a majority.
    net.sim.isolate(2);
    net.state(0)
        .put_template(&segments("lobby/a.txt"), false, b"hello")
        .await
        .unwrap();
    assert!(net.state(1).blobs().has(&hash));
    assert!(!net.state(2).blobs().has(&hash));
    net.sim.heal();
    for index in 0..3 {
        file_becomes(net.storage(index).join("lobby/a.txt"), "hello".to_owned()).await;
    }
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn materialization_follows_uploads_and_deletes_on_every_peer() {
    let mut net = Net::form(3).await;
    net.start_with(&source("a")).await;
    net.state(1)
        .put_template(&segments("lobby/a.txt"), false, b"one")
        .await
        .unwrap();
    net.state(1)
        .put_template(&segments("lobby/plugins/b.jar"), false, b"two")
        .await
        .unwrap();
    net.state(0)
        .put_template(&segments("lobby/world"), true, b"")
        .await
        .unwrap();
    for index in 0..3 {
        file_becomes(net.storage(index).join("lobby/a.txt"), "one".to_owned()).await;
        file_becomes(
            net.storage(index).join("lobby/plugins/b.jar"),
            "two".to_owned(),
        )
        .await;
        let world = net.storage(index).join("lobby/world");
        until(WAIT, move || world.is_dir()).await;
    }
    // A file under a path that is a file is refused.
    let error = net
        .state(2)
        .put_template(&segments("lobby/a.txt/x"), false, b"x")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    net.state(2)
        .delete_template(&segments("lobby/a.txt"))
        .await
        .unwrap();
    for index in 0..3 {
        let removed = net.storage(index).join("lobby/a.txt");
        until(WAIT, move || !removed.exists()).await;
        assert!(net.storage(index).join("lobby/plugins/b.jar").is_file());
    }
    let error = net
        .state(2)
        .delete_template(&segments("lobby/a.txt"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    net.state(0)
        .delete_template(&segments("lobby"))
        .await
        .unwrap();
    for index in 0..3 {
        let removed = net.storage(index).join("lobby");
        until(WAIT, move || !removed.exists()).await;
    }
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_peer_that_installs_a_snapshot_receives_the_configuration_and_templates() {
    let mut net = Net::new(4);
    net.sim.init(0).await;
    for index in 1..3 {
        net.sim.start(index).await.unwrap();
        net.sim
            .admin(0, &["add", &net.sim.name(index), &net.sim.address(index)])
            .await
            .unwrap();
        net.sim
            .retry_admin(0, &["promote", &net.sim.name(index)])
            .await;
    }
    for index in 0..3 {
        net.enabled(index).await;
        fs::write(net.path(index), source("a")).unwrap();
        net.attach(index).await;
        net.state(index).report_running(&source("a"));
    }
    let leader = net.sim.leader().await;
    let id = net
        .state(leader)
        .deploy(&source("b"), &net.path(leader), "tester", "save")
        .await
        .unwrap();
    net.state(leader).report_running(&source("b"));
    net.state(leader)
        .put_template(
            &segments("lobby/server.properties"),
            false,
            b"motd=cluster\n",
        )
        .await
        .unwrap();
    let cluster = net.sim.peer(leader).clone();
    let raft = cluster.raft().unwrap().clone();
    raft.trigger().snapshot().await.unwrap();
    let applied = cluster.state().applied_index();
    until(WAIT, || {
        cluster
            .metrics()
            .and_then(|metrics| metrics.purged)
            .is_some_and(|purged| purged.index >= applied)
    })
    .await;
    // p3 has no rift.lua and no templates yet.
    net.sim.start(3).await.unwrap();
    net.sim.retry_admin(leader, &["add", "p3", "p3:7444"]).await;
    let late = net.sim.peer(3).clone();
    until(WAIT, move || {
        late.state()
            .read(config::latest)
            .is_ok_and(|latest| latest.is_some_and(|latest| latest.id == id))
    })
    .await;
    assert!(net.sim.peer(3).metrics().unwrap().snapshot.is_some());
    assert_eq!(
        startup(net.sim.peer(3), &net.path(3), false).await.unwrap(),
        Startup::Received { id }
    );
    assert_eq!(fs::read_to_string(net.path(3)).unwrap(), source("b"));
    net.attach(3).await;
    file_becomes(
        net.storage(3).join("lobby/server.properties"),
        "motd=cluster\n".to_owned(),
    )
    .await;
    net.shutdown().await;
}

#[test]
fn listings_follow_uploads_folders_and_deletes() {
    let record = |path: &str| FileRecord {
        path: path.into(),
        size: 1,
        sha256: "b".repeat(64),
    };
    let (files, directories) = with_file("lobby", None, record("a/b.txt"))
        .unwrap()
        .unwrap();
    let folder = Folder {
        version: 3,
        files,
        directories,
    };
    assert!(
        with_directory("lobby", Some(&folder), "a")
            .unwrap()
            .is_none()
    );
    let (_, directories) = with_directory("lobby", Some(&folder), "c/d")
        .unwrap()
        .unwrap();
    assert_eq!(directories, ["c/d"]);
    assert_eq!(
        with_file("lobby", Some(&folder), record("a"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::IsADirectory
    );
    assert_eq!(
        with_file("lobby", Some(&folder), record("a/b.txt/c"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    // Deleting the only file keeps its folder, as on a single Rift.
    let (files, directories) = without(&folder, "a/b.txt").unwrap();
    assert!(files.is_empty());
    assert_eq!(directories, ["a"]);
    assert!(without(&folder, "missing").is_none());
}
