//! Named fixed-delay jobs, and the `rift.permissions.on_change` handlers of
//! API v3. A job name cannot overlap, even across reload generations.
use super::{Context, Extensions, PERMISSION_EVENTS, fields, invalid, token};
use crate::permissions::Notification;
use mlua::{Function, Table, Value};
use rift_permissions::{Actor, ChangeEvent};
use std::{
    collections::BTreeMap,
    io,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{
        Semaphore,
        broadcast::{self, error::RecvError},
    },
    task::JoinHandle,
};

const JOB_TIMEOUT: Duration = Duration::from_secs(5);
/// Jobs run on their own workers: a job waiting seconds on an HTTP call must
/// not take a worker that a login or command callback needs within 50 ms.
pub(super) const MAX_CONCURRENT_JOBS: usize = 2;

pub(super) fn parse(value: Value) -> mlua::Result<BTreeMap<String, Duration>> {
    let mut jobs = BTreeMap::new();
    if value.is_nil() {
        return Ok(jobs);
    }
    let Value::Table(entries) = value else {
        return Err(mlua::Error::runtime("extensions.jobs must be a table"));
    };
    for entry in entries.pairs::<Value, Table>() {
        let (name, job) = entry?;
        let Value::String(name) = name else {
            return Err(mlua::Error::runtime("job name must be a string"));
        };
        let name = name.to_str()?.to_owned();
        if !token(&name) || jobs.len() >= 32 {
            return Err(mlua::Error::runtime(
                "invalid job name or more than 32 jobs",
            ));
        }
        fields(&job, &["every_ms", "run"])?;
        let every = match job.raw_get::<Value>("every_ms")? {
            Value::Integer(every) if (100..=86_400_000).contains(&every) => every as u64,
            _ => {
                return Err(mlua::Error::runtime(
                    "job every_ms must be an integer in 100..=86400000",
                ));
            }
        };
        job.raw_get::<Function>("run")?;
        jobs.insert(name, Duration::from_millis(every));
    }
    Ok(jobs)
}

/// Owns scheduling loops. Drop/shutdown prevents new work; already executing
/// callbacks keep their deadline and retain their concurrency permits.
pub struct Scheduler {
    tasks: Vec<JoinHandle<()>>,
}

impl Scheduler {
    pub async fn shutdown(mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Extensions {
    /// Start this committed generation's jobs. Retain the returned owner until
    /// reload/shutdown, and stop the old owner before starting its replacement.
    pub fn scheduler(&self) -> Scheduler {
        let mut tasks = Vec::new();
        if let Some(script) = &self.script {
            let mut gates = self.job_gates.lock().unwrap();
            gates.retain(|_, gate| gate.strong_count() > 0);
            for (name, every) in &script.jobs {
                let gate = gates
                    .get(name)
                    .and_then(std::sync::Weak::upgrade)
                    .unwrap_or_else(|| Arc::new(Semaphore::new(1)));
                gates.insert(name.clone(), Arc::downgrade(&gate));
                let runtime = self.clone();
                let name = name.clone();
                let every = *every;
                tasks.push(tokio::spawn(async move {
                    loop {
                        // First execution waits a full interval. Waiting after completion
                        // avoids catch-up bursts and an unbounded queue of missed ticks.
                        tokio::time::sleep(every).await;
                        if let Err(error) = runtime.run_job(&name, &gate).await {
                            eprintln!("rift: extension job {name}: {error}");
                        }
                    }
                }));
            }
            if script.permission_events {
                // Subscribe now so no change committed after this call is missed.
                let notifications = self.permissions.subscribe();
                tasks.push(tokio::spawn(self.clone().permission_events(notifications)));
            }
        }
        Scheduler { tasks }
    }

    /// Feed committed permission changes to `on_permission_change`, one at a
    /// time on the job workers. Changes made by the handlers themselves are
    /// not fed back.
    async fn permission_events(self, mut notifications: broadcast::Receiver<Notification>) {
        let own = Actor::Lua(PERMISSION_EVENTS.into());
        loop {
            match notifications.recv().await {
                Ok(Notification::Committed { event, .. }) if event.actor != own => {
                    if let Err(error) = self.run_permission_change(event).await {
                        eprintln!("rift: {error}");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(missed)) => eprintln!(
                    "rift: extensions.on_permission_change fell behind and missed {missed} changes"
                ),
                Err(RecvError::Closed) => return,
            }
        }
    }

    async fn run_permission_change(&self, event: Arc<ChangeEvent>) -> io::Result<()> {
        let permit = self
            .job_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(invalid)?;
        let runtime = self.clone();
        let deadline = Instant::now() + JOB_TIMEOUT;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            runtime
                .script
                .as_ref()
                .unwrap()
                .permission_changed(&event, deadline, &runtime)
        });
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task)
            .await
            .map_err(invalid)?
            .map_err(invalid)?
    }

    async fn run_job(&self, name: &str, gate: &Arc<Semaphore>) -> io::Result<()> {
        // An old callback may still be stopping after a timeout or reload.
        let Ok(job) = gate.clone().try_acquire_owned() else {
            return Ok(());
        };
        let permit = self
            .job_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| invalid("extension job capacity exhausted; tick skipped"))?;
        let runtime = self.clone();
        let input = Context {
            command: Some(name.into()),
            ..Context::default()
        };
        let deadline = Instant::now() + JOB_TIMEOUT;
        let task = tokio::task::spawn_blocking(move || {
            let _job = job;
            let _permit = permit;
            runtime
                .script
                .as_ref()
                .unwrap()
                .evaluate("job", &input, deadline, &runtime)
        });
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task)
            .await
            .map_err(invalid)?
            .map_err(invalid)??;
        Ok(())
    }
}
