//! In-memory, bounded results for HTTP instance operations. Pending operations
//! keep their slots until the control service replies; disconnecting never
//! cancels a transaction or releases its tracking slot.
use crate::control;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 32;
const MAX_RETAINED: usize = 256;
const RETENTION: Duration = Duration::from_secs(600);

pub(super) struct Record {
    pub group: Option<String>,
    pub value: Value,
    completed: Option<Instant>,
}

#[derive(Default)]
pub(super) struct Store(Mutex<BTreeMap<String, Record>>);

impl Store {
    pub fn reserve(
        &self,
        group: Option<String>,
        action: &str,
        target: &str,
    ) -> Result<String, control::Error> {
        let mut records = self.0.lock().unwrap();
        Self::prune(&mut records, Instant::now());
        if records.len() >= MAX_RETAINED
            || records.values().filter(|r| r.completed.is_none()).count() >= MAX_PENDING
        {
            return Err(control::Error {
                status: 503,
                message: "instance operation tracking capacity exhausted; try again later".into(),
            });
        }
        let mut random = [0; 16];
        aws_lc_rs::rand::fill(&mut random).map_err(|_| control::Error {
            status: 500,
            message: "could not allocate an operation ID".into(),
        })?;
        let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        records.insert(id.clone(), Record {
            group,
            value: json!({"operation_id":id,"operation":action,"target":target,"status":"pending"}),
            completed: None,
        });
        Ok(id)
    }

    pub fn discard(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }

    pub fn complete(&self, id: &str, reply: control::Reply, success_status: u16) {
        let reply = reply.and_then(|result| {
            serde_json::from_str::<Value>(&result).map_err(|_| control::Error {
                status: 500,
                message: "invalid instance operation response".into(),
            })
        });
        let mut records = self.0.lock().unwrap();
        if let Some(record) = records.get_mut(id) {
            match reply {
                Ok(result) => {
                    record.value["status"] = json!("succeeded");
                    record.value["http_status"] = json!(success_status);
                    record.value["result"] = result;
                }
                Err(error) => {
                    record.value["status"] = json!("failed");
                    record.value["http_status"] = json!(error.status);
                    record.value["error"] =
                        json!(error.message.chars().take(2048).collect::<String>());
                }
            }
            record.completed = Some(Instant::now());
        }
    }

    pub fn get(
        &self,
        id: &str,
        group_allowed: impl FnOnce(Option<&str>) -> bool,
    ) -> Result<Value, control::Error> {
        let mut records = self.0.lock().unwrap();
        Self::prune(&mut records, Instant::now());
        let record = records.get(id).ok_or_else(|| control::Error {
            status: 404,
            message: "operation not found or no longer retained".into(),
        })?;
        if !group_allowed(record.group.as_deref()) {
            return Err(control::Error {
                status: 403,
                message: "operator group scope denied".into(),
            });
        }
        Ok(record.value.clone())
    }

    fn prune(records: &mut BTreeMap<String, Record>, now: Instant) {
        records.retain(|_, record| {
            record
                .completed
                .is_none_or(|completed| now.duration_since(completed) < RETENTION)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_and_retained_results_are_bounded_and_expire_only_after_completion() {
        let store = Store::default();
        let ids: Vec<_> = (0..MAX_PENDING)
            .map(|_| {
                store
                    .reserve(Some("lobby".into()), "remove", "lobby-1")
                    .unwrap()
            })
            .collect();
        assert_eq!(
            store.reserve(None, "create", "lobby").unwrap_err().status,
            503
        );
        Store::prune(&mut store.0.lock().unwrap(), Instant::now() + RETENTION);
        assert_eq!(store.0.lock().unwrap().len(), MAX_PENDING);
        for id in &ids {
            store.complete(id, Ok("{\"removed\":true}".into()), 200);
        }
        for _ in MAX_PENDING..MAX_RETAINED {
            let id = store.reserve(None, "create", "lobby").unwrap();
            store.complete(&id, Ok("{}".into()), 201);
        }
        assert_eq!(
            store.reserve(None, "create", "lobby").unwrap_err().status,
            503
        );
        Store::prune(&mut store.0.lock().unwrap(), Instant::now() + RETENTION);
        assert!(store.0.lock().unwrap().is_empty());
        assert_eq!(store.get(&ids[0], |_| true).unwrap_err().status, 404);
        store.reserve(None, "create", "lobby").unwrap();
    }

    #[test]
    fn failure_and_saved_group_are_available_after_completion() {
        let store = Store::default();
        let id = store
            .reserve(Some("lobby".into()), "remove", "lobby-1")
            .unwrap();
        store.complete(&id, Err(control::Error::conflict("instance occupied")), 200);
        assert_eq!(store.get(&id, |_| false).unwrap_err().status, 403);
        let value = store.get(&id, |group| group == Some("lobby")).unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["http_status"], 409);
        assert_eq!(value["error"], "instance occupied");
        assert_eq!(store.get("unknown", |_| true).unwrap_err().status, 404);
        store.discard(&id);
        assert_eq!(store.get(&id, |_| true).unwrap_err().status, 404);
    }
}
