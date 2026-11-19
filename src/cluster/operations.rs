//! Writes to the instance database in a cluster (docs/clustering.md,
//! "Writes to the instance database").
//!
//! [`ClusterExecutor`] is the database's [`Executor`] while a cluster runs:
//! [`crate::database::Database::run`] hands it every typed [`Operation`].
//!
//! - `kv` and `directory` operations are proposed as entries of their own,
//!   which every peer executes deterministically when they apply.
//! - Every other operation goes to the leader: a request on the peer link,
//!   or this peer's own queue when it leads. The leader takes the queued
//!   operations one after another into one captured transaction, each in its
//!   own savepoint (a failing one rolls back alone), proposes one
//!   `operation` entry with the outcomes and the changeset, and keeps
//!   executing on top of the changesets still in flight (group commit).
//!   `migrate` operations wait until nothing is in flight and are proposed
//!   as `migrate` entries.
//!
//! The caller gets its [`Outcome`] once the entry is applied on its own peer,
//! so it reads its own write, and after its hooks ran there. Each operation
//! carries `(peer, incarnation, serial)`: a retry after a leader change
//! returns the outcome stored when the first attempt applied. Operations of
//! leader tasks carry the term they were decided in ([`Cluster::run_in_term`])
//! and are refused by a leader of another term.
use super::{
    Cluster, PeerId, Raft, TypeConfig, hex,
    network::{RaftRequest, RaftResponse},
    node::{self, ExecuteError, PROPOSE_TIMEOUT},
    replicated::{self, Batch, Executed, Pending},
    state::{
        Entry, EntryKind, Origin, Payload, Proposal, Reply, applied_log_id, sqlite, stored_reply,
    },
    transport::stopped,
    wall_clock_ms,
};
use crate::database::{
    self, Conn, Database, Executor, Operation, Outcome,
    changeset::{self, Changeset},
    operation::{MigrateOp, execute_in_savepoint},
};
use openraft::{
    ServerState, errors::ClientWriteError, impls::ProgressResponder, raft::ClientWriteResult,
};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    io,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    runtime::{Handle, RuntimeFlavor},
    sync::{mpsc, oneshot},
    time::Instant,
};

/// Most operations the leader takes into one entry.
const MAX_BATCH: usize = 256;

/// An operation sent to the leader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub origin: Origin,
    /// For leader tasks: the term the operation was decided in.
    pub term: Option<u64>,
    pub operation: Operation,
}

/// The leader's answer to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Applied on the leader at `index` (or earlier, for a retry).
    Done { index: u64, outcome: Outcome },
    /// Ask this leader instead, if known.
    NotLeader { leader: Option<PeerId> },
    /// Not executed or not known to be applied; retry with the same origin.
    Retry(String),
    /// Refused for good, such as an operation decided in another term.
    Refused(String),
}

struct Job {
    request: Request,
    reply: oneshot::Sender<Response>,
}

impl Job {
    fn answer(self, response: Response) {
        // The caller may have given up.
        let _ = self.reply.send(response);
    }
}

fn answer_all(jobs: Vec<Job>, response: &Response) {
    for job in jobs {
        job.answer(response.clone());
    }
}

/// The leader's queue of operations, drained by [`Cluster::lead`].
pub(super) struct Queue {
    sender: mpsc::UnboundedSender<Job>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Job>>>,
}

impl Queue {
    pub(super) fn new() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

/// What the leader decided for each operation of a batch.
enum Planned {
    /// Applied before (a retry): its stored reply, applied by `index`.
    Stored {
        index: u64,
        reply: Reply,
    },
    /// The same operation is in flight already.
    InFlight,
    Executed(Outcome),
}

struct Plan {
    results: Vec<Planned>,
    /// `None` when nothing was executed.
    changeset: Option<Changeset>,
}

/// Execute what needs executing among `requests` on the writer, on top of
/// the pending changesets, and record the batch as pending under `origin`.
fn plan(
    conn: &mut Conn,
    pending: &mut Pending,
    origin: &Origin,
    requests: &[(Origin, Operation)],
) -> io::Result<Plan> {
    let mut results = Vec::with_capacity(requests.len());
    let mut execute: Vec<usize> = Vec::new();
    {
        let sql = sqlite(conn)?;
        let applied = applied_log_id(sql)?.map_or(0, |log_id| log_id.index);
        for (position, (op_origin, _)) in requests.iter().enumerate() {
            if let Some(reply) = stored_reply(sql, op_origin)? {
                results.push(Planned::Stored {
                    index: applied,
                    reply,
                });
                continue;
            }
            let duplicate = pending.holds(op_origin)
                || execute.iter().any(|other| requests[*other].0 == *op_origin);
            // Executed ones get their outcome below.
            results.push(Planned::InFlight);
            if !duplicate {
                execute.push(position);
            }
        }
    }
    if execute.is_empty() {
        return Ok(Plan {
            results,
            changeset: None,
        });
    }
    let base = pending.changesets();
    let (outcomes, captured) = changeset::capture_on(conn, &base, |transaction| {
        execute
            .iter()
            .map(|position| execute_in_savepoint(transaction, &requests[*position].1))
            .collect::<database::Result<Vec<Outcome>>>()
    })
    .map_err(|error| io::Error::other(format!("cluster: executing operations: {error}")))?;
    let outcomes = outcomes
        .map_err(|error| io::Error::other(format!("cluster: executing operations: {error}")))?;
    for (position, outcome) in execute.iter().zip(outcomes) {
        results[*position] = Planned::Executed(outcome);
    }
    pending.insert(
        origin.clone(),
        captured.clone(),
        execute
            .iter()
            .map(|position| requests[*position].0.clone())
            .collect(),
    );
    Ok(Plan {
        results,
        changeset: Some(captured),
    })
}

impl Cluster {
    /// Run `operation` on the instance database of the cluster and return
    /// its outcome once the change is applied on this peer.
    pub async fn run_operation(&self, operation: Operation) -> io::Result<Outcome> {
        self.operate(operation, None, None).await
    }

    /// [`Cluster::run_operation`] for leader tasks: `term` is the term the
    /// task decided in (its [`super::node::LeaderContext::term`]). A leader
    /// of another term refuses it, so a deposed leader's decision never
    /// takes effect through the new one.
    pub async fn run_in_term(&self, operation: Operation, term: u64) -> io::Result<Outcome> {
        self.operate(operation, Some(term), None).await
    }

    /// Run `operation`, giving up after `limit` (at most
    /// [`PROPOSE_TIMEOUT`]) with [`io::ErrorKind::TimedOut`].
    pub(super) async fn operate(
        &self,
        operation: Operation,
        term: Option<u64>,
        limit: Option<Duration>,
    ) -> io::Result<Outcome> {
        if self.raft().is_none() {
            return Err(node::not_joined());
        }
        let timeout = limit.map_or(PROPOSE_TIMEOUT, |limit| limit.min(PROPOSE_TIMEOUT));
        if let Some(entry) = replicated::own_entry(&operation)? {
            let (reply, _) = tokio::time::timeout(timeout, self.propose_entry(entry, term))
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "cluster: the write was not applied in time",
                    )
                })??;
            return replicated::outcome_of(reply);
        }
        let request = Request {
            origin: self.next_origin(),
            term,
            operation,
        };
        self.submit_operation(request, timeout).await
    }

    /// Get `request` executed by the leader, retrying (with the same origin)
    /// through leader changes until `timeout`, then wait until it is
    /// applied here.
    pub(super) async fn submit_operation(
        &self,
        request: Request,
        timeout: Duration,
    ) -> io::Result<Outcome> {
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(25);
        loop {
            let reason = match self.attempt_operation(&request, deadline).await {
                Response::Done { index, outcome } => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    self.state()
                        .wait_applied(index, remaining.max(Duration::from_secs(1)))
                        .await?;
                    return Ok(outcome);
                }
                Response::Refused(message) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("cluster: {message}"),
                    ));
                }
                Response::NotLeader { leader: None } => "no leader with a quorum".to_owned(),
                Response::NotLeader {
                    leader: Some(leader),
                } => {
                    format!("raft id {leader} leads now")
                }
                Response::Retry(reason) => reason,
            };
            if Instant::now() + delay >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("cluster: the write was not applied in time: {reason}"),
                ));
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(500));
        }
    }

    async fn attempt_operation(&self, request: &Request, deadline: Instant) -> Response {
        let me = self.raft_id();
        let Some(leader) = self.metrics().and_then(|metrics| metrics.current_leader) else {
            return Response::NotLeader { leader: None };
        };
        if Some(leader) == me {
            return self.enqueue(request.clone()).await;
        }
        let Some(name) = self.peer_name(leader) else {
            return Response::Retry(format!("leader {leader} has no known address"));
        };
        let timeout = deadline.saturating_duration_since(Instant::now());
        match self
            .raft_request(&name, &RaftRequest::Operate(request.clone()), timeout)
            .await
        {
            Ok(RaftResponse::Operated(response)) => response,
            Ok(_) => Response::Retry(format!("unexpected answer from {name}")),
            Err(error) => Response::Retry(error.to_string()),
        }
    }

    /// Hand `request` to this peer's leader queue and wait for the answer.
    async fn enqueue(&self, request: Request) -> Response {
        let (reply, answer) = oneshot::channel();
        if self
            .inner
            .operations
            .sender
            .send(Job { request, reply })
            .is_err()
        {
            return Response::Retry("this peer is stopping".into());
        }
        answer
            .await
            .unwrap_or_else(|_| Response::Retry("the leader's queue stopped".into()))
    }

    /// A follower's request: executed only while this peer leads.
    pub(super) async fn serve_operation(&self, request: Request) -> Response {
        self.enqueue(request).await
    }

    /// Drain the leader queue: operations that arrive while a batch is
    /// executed or proposed form the next batch.
    pub(super) async fn lead(self) {
        let receiver = self.inner.operations.receiver.lock().unwrap().take();
        let Some(mut jobs) = receiver else { return };
        let mut stop = self.inner.stop.subscribe();
        let mut synced = None;
        loop {
            let first = tokio::select! {
                job = jobs.recv() => match job {
                    Some(job) => job,
                    None => return,
                },
                _ = stopped(&mut stop) => return,
            };
            let mut batch = vec![first];
            while batch.len() < MAX_BATCH {
                match jobs.try_recv() {
                    Ok(job) => batch.push(job),
                    Err(_) => break,
                }
            }
            self.execute_jobs(batch, &mut synced).await;
        }
    }

    async fn execute_jobs(&self, jobs: Vec<Job>, synced: &mut Option<u64>) {
        let (Some(raft), Some(metrics)) = (self.raft(), self.metrics()) else {
            answer_all(jobs, &Response::Retry("not in a cluster yet".into()));
            return;
        };
        let me = metrics.id;
        if metrics.state != ServerState::Leader || metrics.current_leader != Some(me) {
            let leader = metrics.current_leader.filter(|leader| *leader != me);
            answer_all(jobs, &Response::NotLeader { leader });
            return;
        }
        let term = metrics.current_term;
        if *synced != Some(term) {
            // What this peer proposed in an earlier term is committed or
            // lost; once everything committed before this term is applied,
            // the replica (and its stored outcomes) says which.
            let _ = self.state().with_writer(|_, pending| {
                pending.clear();
                Ok(())
            });
            let index = match self.local_read_index().await {
                Ok(index) => index,
                Err(ExecuteError::NotLeader(leader)) => {
                    answer_all(jobs, &Response::NotLeader { leader });
                    return;
                }
                Err(ExecuteError::Retry(reason)) => {
                    answer_all(jobs, &Response::Retry(reason));
                    return;
                }
                Err(ExecuteError::Fatal(error)) => {
                    answer_all(jobs, &Response::Retry(error.to_string()));
                    return;
                }
            };
            if let Err(error) = self
                .state()
                .wait_applied(index, Duration::from_secs(10))
                .await
            {
                answer_all(jobs, &Response::Retry(error.to_string()));
                return;
            }
            *synced = Some(term);
        }
        match self.is_enabled(EntryKind::Operation) {
            Ok(true) => {}
            Ok(false) => {
                answer_all(
                    jobs,
                    &Response::Retry("operation entries are not enabled yet".into()),
                );
                return;
            }
            Err(error) => {
                answer_all(jobs, &Response::Retry(error.to_string()));
                return;
            }
        }
        let mut batch = Vec::new();
        for job in jobs {
            if let Some(decided) = job.request.term
                && decided != term
            {
                job.answer(Response::Refused(format!(
                    "decided in term {decided}, but this leader is in term {term}; leadership changed"
                )));
                continue;
            }
            if let Operation::Migrate(op) = &job.request.operation {
                let entry = replicated::migrate_entry(op);
                self.flush(std::mem::take(&mut batch), term, raft).await;
                match entry {
                    Ok(entry) => self.migrate(job, entry, term).await,
                    Err(error) => job.answer(Response::Refused(error.to_string())),
                }
            } else {
                batch.push(job);
            }
        }
        self.flush(batch, term, raft).await;
    }

    /// Execute `jobs` as one batch and propose its `operation` entry; the
    /// jobs are answered once it is applied here.
    async fn flush(&self, jobs: Vec<Job>, term: u64, raft: &Raft) {
        if jobs.is_empty() {
            return;
        }
        let origin = self.next_origin();
        let requests: Vec<(Origin, Operation)> = jobs
            .iter()
            .map(|job| (job.request.origin.clone(), job.request.operation.clone()))
            .collect();
        let plan = match self
            .state()
            .with_writer(|conn, pending| plan(conn, pending, &origin, &requests))
        {
            Ok(plan) => plan,
            Err(error) => {
                answer_all(jobs, &Response::Retry(error.to_string()));
                return;
            }
        };
        let mut waiting = Vec::new();
        for (job, result) in jobs.into_iter().zip(plan.results) {
            match result {
                Planned::Stored { index, reply } => {
                    job.answer(match replicated::outcome_of(reply) {
                        Ok(outcome) => Response::Done { index, outcome },
                        Err(error) => Response::Refused(error.to_string()),
                    })
                }
                Planned::InFlight => {
                    job.answer(Response::Retry("the same operation is in flight".into()))
                }
                Planned::Executed(outcome) => waiting.push((job, outcome)),
            }
        }
        let Some(changeset) = plan.changeset else {
            return;
        };
        let batch = Batch {
            ops: waiting
                .iter()
                .map(|(job, outcome)| Executed {
                    origin: job.request.origin.clone(),
                    operation: job.request.operation.clone(),
                    outcome: outcome.clone(),
                })
                .collect(),
            changeset: hex(&changeset.0),
        };
        let data = match serde_json::to_value(&batch) {
            Ok(data) => data,
            Err(error) => {
                self.state().forget_pending(&origin);
                for (job, _) in waiting {
                    job.answer(Response::Retry(format!("encoding the entry: {error}")));
                }
                return;
            }
        };
        let proposal = Proposal {
            origin: Some(origin.clone()),
            time_ms: wall_clock_ms(),
            // Computed against this term's state: applied only if appended
            // in this term.
            term: Some(term),
            entry: Entry::Operation(Payload::new(replicated::VERSION, data)),
        };
        let (responder, applied) =
            ProgressResponder::<TypeConfig, ClientWriteResult<TypeConfig>>::complete_only();
        // Returns once the entry is queued in order; the next batch builds
        // on it while it commits.
        if let Err(fatal) = raft.client_write_ff(proposal, Some(responder)).await {
            self.state().forget_pending(&origin);
            for (job, _) in waiting {
                job.answer(Response::Retry(format!("raft stopped: {fatal}")));
            }
            return;
        }
        let state = self.state().clone();
        let me = self.raft_id();
        tokio::spawn(async move {
            let result = match applied.await {
                Ok(Ok(written)) => match written.data {
                    Reply::Ok(_) => Ok(written.log_id.index),
                    Reply::Err(message) => Err(Response::Retry(message)),
                },
                Ok(Err(error)) => {
                    state.forget_pending(&origin);
                    Err(match error {
                        ClientWriteError::ForwardToLeader(forward) => Response::NotLeader {
                            leader: forward.leader_id.filter(|leader| Some(*leader) != me),
                        },
                        other => Response::Retry(other.to_string()),
                    })
                }
                Err(_) => {
                    state.forget_pending(&origin);
                    Err(Response::Retry("raft dropped the proposal".into()))
                }
            };
            for (job, outcome) in waiting {
                job.answer(match &result {
                    Ok(index) => Response::Done {
                        index: *index,
                        outcome,
                    },
                    Err(response) => response.clone(),
                });
            }
        });
    }

    /// Propose a `migrate` entry once nothing computed on the old schema is
    /// in flight. Its origin is the operation's, so a retry gets the stored
    /// outcome.
    async fn migrate(&self, job: Job, entry: Entry, term: u64) {
        let deadline = Instant::now() + PROPOSE_TIMEOUT;
        while self.state().has_pending() {
            if Instant::now() >= deadline {
                job.answer(Response::Retry(
                    "operation entries are still in flight".into(),
                ));
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let proposal = Proposal {
            origin: Some(job.request.origin.clone()),
            time_ms: 0,
            term: Some(term),
            entry,
        };
        let response = match self.execute(proposal).await {
            Ok((reply, index)) => match replicated::outcome_of(reply) {
                Ok(outcome) => Response::Done { index, outcome },
                Err(error) => Response::Retry(error.to_string()),
            },
            Err(ExecuteError::NotLeader(leader)) => Response::NotLeader { leader },
            Err(ExecuteError::Retry(reason)) => Response::Retry(reason),
            Err(ExecuteError::Fatal(error)) => Response::Retry(error.to_string()),
        };
        job.answer(response);
    }

    /// On the leader: migrate every component whose schema is older than
    /// this build writes, once a committed `features` entry says every
    /// member supports the new version.
    pub(super) async fn migrate_schemas(&self, term: u64) -> io::Result<()> {
        let database = self.state().database().clone();
        let behind = tokio::task::spawn_blocking(move || replicated::schema_behind(&database))
            .await
            .map_err(io::Error::other)??;
        if behind.is_empty() {
            return Ok(());
        }
        let Some(features) = self.state().features()? else {
            return Ok(());
        };
        for (component, from, latest) in behind {
            if !features
                .kinds
                .contains(&replicated::schema_feature(component, latest))
            {
                continue;
            }
            log_info!("cluster: migrating {component} from schema version {from} to {latest}");
            let op = Operation::Migrate(MigrateOp {
                component: component.to_owned(),
                version: latest,
                now: wall_clock_ms() / 1000,
            });
            let outcome = self.run_in_term(op, term).await?;
            if outcome.failed() {
                log_error!("cluster: migrating {component} failed: {outcome:?}");
            }
        }
        Ok(())
    }
}

/// The instance database's [`Executor`] while a cluster runs. It holds the
/// cluster weakly: the database owns its executor, and the cluster the
/// database.
pub struct ClusterExecutor {
    cluster: Weak<node::Inner>,
    runtime: Handle,
}

impl ClusterExecutor {
    /// An executor for `cluster`; call it inside the runtime the cluster
    /// runs on.
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            cluster: Arc::downgrade(&cluster.inner),
            runtime: Handle::current(),
        }
    }
}

impl Executor for ClusterExecutor {
    fn run(
        &self,
        _database: &Database,
        operation: Operation,
        deadline: Option<std::time::Instant>,
    ) -> io::Result<Outcome> {
        let cluster = self
            .cluster
            .upgrade()
            .map(|inner| Cluster { inner })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "cluster: stopped"))?;
        let limit = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "script deadline exceeded",
                    ));
                }
                Some(left)
            }
            None => None,
        };
        // A leader task's write carries the term it was decided in.
        let term = super::leader::current_term();
        block_on(&self.runtime, async move {
            cluster.operate(operation, term, limit).await
        })
    }
}

/// Run `work` on `runtime` and wait for it on this thread. Callers of
/// [`Database::run`] are synchronous; on a runtime worker the wait moves
/// off the worker first. On a current-thread runtime, call from a blocking
/// thread (`spawn_blocking`).
fn block_on<T: Send + 'static>(
    runtime: &Handle,
    work: impl Future<Output = io::Result<T>> + Send + 'static,
) -> io::Result<T> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let _ = sender.send(work.await);
    });
    let wait = move || {
        receiver.recv().unwrap_or_else(|_| {
            Err(io::Error::other(
                "cluster: the write was dropped; this peer is stopping",
            ))
        })
    };
    match Handle::try_current() {
        Ok(current) if current.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(wait)
        }
        _ => wait(),
    }
}
