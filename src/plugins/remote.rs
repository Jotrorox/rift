//! Remote mode: bus responders configured in `extensions.decisions`, the last
//! position of a decision chain. The request and reply wire format is the
//! one in `extensions/decisions.rs`, unchanged.
use crate::{
    extensions::{Action, Context, decisions::Decision},
    messaging::Broker,
};
use std::{collections::BTreeMap, io, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Clone, Default)]
pub(crate) struct Remote {
    decisions: BTreeMap<String, Decision>,
    /// Open requests per hook, kept across reloads while the limit is unchanged.
    slots: Arc<BTreeMap<String, Arc<Semaphore>>>,
    broker: Option<Broker>,
}

impl Remote {
    pub(crate) fn new(
        decisions: &BTreeMap<String, Decision>,
        broker: Broker,
        previous: Option<&Self>,
    ) -> Self {
        let slots = decisions
            .iter()
            .map(|(hook, decision)| {
                let reused = previous
                    .and_then(|previous| {
                        previous
                            .decisions
                            .get(hook)
                            .filter(|old| old.max_in_flight == decision.max_in_flight)
                            .and_then(|_| previous.slots.get(hook))
                    })
                    .cloned();
                let slots =
                    reused.unwrap_or_else(|| Arc::new(Semaphore::new(decision.max_in_flight)));
                (hook.clone(), slots)
            })
            .collect();
        Self {
            decisions: decisions.clone(),
            slots: Arc::new(slots),
            broker: Some(broker),
        }
    }

    /// The responder for `hook`, or continue when none is configured.
    pub(crate) async fn decide(&self, hook: &str, input: &Context) -> io::Result<Action> {
        let (Some(decision), Some(broker)) = (self.decisions.get(hook), &self.broker) else {
            return Ok(Action::Continue);
        };
        decision
            .decide(hook, input, broker, &self.slots[hook])
            .await
    }

    /// The hooks a responder is configured for.
    pub(crate) fn hooks(&self) -> impl Iterator<Item = &str> {
        self.decisions.keys().map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn slots(&self, hook: &str) -> Arc<Semaphore> {
        self.slots[hook].clone()
    }
}
