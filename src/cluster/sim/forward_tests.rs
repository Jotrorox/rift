//! Leader-only requests a follower hands the leader (plan step 7): moves,
//! backups and restores and cache warming run on the leader, in its term,
//! for the operator who made them; the leader's answer or refusal comes
//! back, and a peer that does not lead refuses such a request.
//!
//! The store's constructor may write through the cluster, so it runs on a
//! thread of its own, as in the database tests.
use super::Sim;
use crate::{
    cluster::{
        StreamKind,
        leader::{self, Coordination, ForwardedRequest, RequestHandler},
    },
    config::Config,
    instance_store::Store,
    managed::{CoordinatorFuture, ManagedServers},
    players::PlayerRegistry,
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

/// Run `work` on a thread outside the runtime and wait for it.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.expect("the thread finished")
}

/// The requests a peer's handler ran, with the leader term it ran them in.
type Seen = Arc<Mutex<Vec<(ForwardedRequest, Option<u64>)>>>;

/// One peer's coordination, with a handler like the runtime's: it records
/// each request and answers with what it saw, and refuses a backup as a
/// leader without configured backups does.
struct Peer {
    coordination: Coordination,
    seen: Seen,
    _managed: ManagedServers,
    _store: Arc<Store>,
}

async fn coordinate(sim: &Sim, index: usize) -> Peer {
    let database = sim.database(index);
    let store = Arc::new(off_thread(move || Store::new(database)).await.unwrap());
    let managed = ManagedServers::new(&Config::default(), Arc::new(PlayerRegistry::default()));
    let coordination = Coordination::start(sim.peer(index), &managed, &store).unwrap();
    let seen: Seen = Arc::default();
    let name = sim.name(index);
    let recorded = seen.clone();
    let handler: RequestHandler = Arc::new(move |request: ForwardedRequest| {
        let recorded = recorded.clone();
        let name = name.clone();
        let answer: CoordinatorFuture<Result<Value, String>> = Box::pin(async move {
            let term = leader::current_term();
            recorded.lock().unwrap().push((request.clone(), term));
            if request.kind == "backup" {
                let instance = request.body["instance"].as_str().unwrap_or_default();
                return Err(format!("{instance}: no backups"));
            }
            Ok(json!({
                "peer": name,
                "kind": request.kind,
                "actor": request.actor,
                "body": request.body,
                "term": term,
            }))
        });
        answer
    });
    coordination.set_request_handler(handler);
    Peer {
        coordination,
        seen,
        _managed: managed,
        _store: store,
    }
}

fn request(kind: &str, actor: &str, body: Value) -> ForwardedRequest {
    ForwardedRequest {
        kind: kind.to_owned(),
        actor: actor.to_owned(),
        body,
    }
}

#[tokio::test(start_paused = true)]
async fn followers_hand_leader_only_requests_to_the_leader_for_their_operator() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let other = (0..3)
        .find(|index| *index != leader && *index != follower)
        .unwrap();
    let mut peers = Vec::new();
    for index in 0..3 {
        peers.push(coordinate(&sim, index).await);
    }
    let wait = Duration::from_secs(5);
    let term = peers[leader].coordination.acting_term().unwrap();

    // A move a follower received runs on the leader, in its term, for the
    // operator who asked; the leader's answer comes back.
    let body = json!({"server": "survival-1", "node": "b", "copy": true, "keep_source": false});
    let moved = request("move", "alice", body.clone());
    let answer = peers[follower]
        .coordination
        .forward_request(moved.clone(), wait)
        .await
        .unwrap();
    assert_eq!(answer["peer"], sim.name(leader));
    assert_eq!(answer["kind"], "move");
    assert_eq!(answer["actor"], "alice");
    assert_eq!(answer["body"], body);
    assert_eq!(answer["term"], term);
    assert_eq!(
        *peers[leader].seen.lock().unwrap(),
        vec![(moved.clone(), Some(term))]
    );

    // The leader's refusal comes back to the follower, and the actor of
    // every request reaches the leader as the follower named it.
    let backup = request("backup", "web:bob", json!({"instance": "survival-1"}));
    let refused = peers[other]
        .coordination
        .forward_request(backup.clone(), wait)
        .await
        .unwrap_err();
    let message = refused.to_string();
    assert!(message.contains("survival-1: no backups"), "{message}");
    assert_eq!(
        peers[leader].seen.lock().unwrap().last(),
        Some(&(backup, Some(term)))
    );

    // A peer that does not lead refuses a request handed to it, without
    // running its handler.
    let message = json!({"kind": "request", "request": moved});
    let bytes = Bytes::from(message.to_string());
    let links = sim.peer(follower).links();
    let refused = links
        .request(&sim.name(other), StreamKind::Load, bytes, wait)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("does not lead"), "{refused}");
    for index in [follower, other] {
        assert!(peers[index].seen.lock().unwrap().is_empty(), "p{index}");
    }

    // The leader has nobody to hand its own requests to.
    let error = peers[leader]
        .coordination
        .forward_request(moved, wait)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotConnected, "{error}");
    assert_eq!(peers[leader].seen.lock().unwrap().len(), 2);

    for peer in &peers {
        peer.coordination.stop();
    }
    drop(peers);
    sim.shutdown().await;
}
