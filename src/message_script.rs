//! Persistent async subscriptions dispatch into short-lived, sandboxed Lua VMs.
//! Subscription queues and payload ownership remain in Rust across callbacks.

use std::time::Instant;

use mlua::{Function, Value};
use tokio::{
    sync::{Semaphore, watch},
    task::{JoinSet, spawn_blocking},
    time::timeout,
};

use crate::{
    config::{Config, MessagingSubscription},
    messaging::{Broker, Error, Message, Subscription},
};

static SLOTS: Semaphore = Semaphore::const_new(crate::script::MAX_CONCURRENT);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageScript {
    source: crate::script::ScriptSource,
}

impl MessageScript {
    pub(crate) fn new(source: &crate::script::ScriptSource) -> Self {
        Self {
            source: source.clone(),
        }
    }

    async fn execute(&self, message: Message, broker: Broker) -> Result<(), String> {
        // Waiting for a worker leaves subsequent messages in the bounded broker
        // queue. Overload remains visible through the broker's slow-consumer policy.
        let permit = SLOTS.acquire().await.map_err(|error| error.to_string())?;
        let script = self.clone();
        let deadline = Instant::now() + crate::script::EXECUTION_TIMEOUT;
        let task = spawn_blocking(move || {
            let _permit = permit;
            script.evaluate(message, broker, deadline)
        });
        timeout(crate::script::EXECUTION_TIMEOUT, task)
            .await
            .map_err(|_| "message script deadline exceeded".to_owned())?
            .map_err(|error| format!("message script worker: {error}"))?
    }

    fn evaluate(&self, message: Message, broker: Broker, deadline: Instant) -> Result<(), String> {
        let run = || -> mlua::Result<()> {
            let (lua, root) = crate::script::load(&self.source, deadline)?;
            crate::script::install_messaging(&lua, Some(broker), deadline)?;
            let Value::Table(root) = root else {
                return Err(mlua::Error::runtime("configuration must return a table"));
            };
            let hook: Function = root.raw_get("on_message")?;
            let input = lua.create_table()?;
            input.raw_set("subject", message.subject.as_ref())?;
            input.raw_set("payload", lua.create_string(&message.payload)?)?;
            input.raw_set("reply", message.reply.as_deref())?;
            hook.call::<()>(input)?;
            crate::script::check_deadline(deadline)
        };
        run().map_err(|error| {
            format!("{}: on_message: {error}", self.source.entry.name)
                .chars()
                .take(2048)
                .collect()
        })
    }
}

/// Register before advertising readiness. Dropping the run future aborts its
/// child workers and releases subscriptions; bounded Lua jobs finish naturally.
/// The script update channel preserves subscriptions and queued messages on reload.
pub struct MessageHandler {
    subscriptions: Vec<(MessagingSubscription, Subscription)>,
    broker: Broker,
    scripts: watch::Receiver<MessageScript>,
    update: watch::Sender<MessageScript>,
}

impl MessageHandler {
    pub fn new(config: &Config, broker: Broker) -> Result<Option<Self>, Error> {
        let Some(script) = config.on_message.clone() else {
            return Ok(None);
        };
        let subscriptions = config
            .messaging
            .as_ref()
            .into_iter()
            .flat_map(|config| &config.subscriptions)
            .map(|subscription| {
                broker
                    .subscribe(&subscription.subject, subscription.queue.as_deref())
                    .map(|receiver| (subscription.clone(), receiver))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (update, scripts) = watch::channel(script);
        Ok(Some(Self {
            subscriptions,
            broker,
            scripts,
            update,
        }))
    }

    pub fn script_updates(&self) -> watch::Sender<MessageScript> {
        self.update.clone()
    }

    pub async fn run(self) {
        let mut workers = JoinSet::new();
        for (settings, mut subscription) in self.subscriptions {
            let scripts = self.scripts.clone();
            let broker = self.broker.clone();
            workers.spawn(async move {
                loop {
                    let message = match subscription.recv().await {
                        Ok(message) => message,
                        Err(Error::SlowConsumer) => {
                            eprintln!("messaging Lua subscription {} exceeded its queue; dropped pending messages and resubscribing", settings.subject);
                            match broker.subscribe(&settings.subject, settings.queue.as_deref()) {
                                Ok(receiver) => subscription = receiver,
                                Err(error) => {
                                    eprintln!("messaging Lua subscription ended: {error}");
                                    break;
                                }
                            }
                            continue;
                        }
                        Err(error) => {
                            eprintln!("messaging Lua subscription ended: {error}");
                            break;
                        }
                    };
                    let script = scripts.borrow().clone();
                    if let Err(error) = script.execute(message, broker.clone()).await {
                        eprintln!("{error}");
                    }
                }
            });
        }
        while workers.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    fn config(body: &str) -> Config {
        Config::from_lua(&format!(r#"
            local calls = 0
            return {{
                listeners = {{ public = '127.0.0.1:0' }},
                backends = {{ lobby = '127.0.0.1:25566' }},
                routes = {{ public = 'lobby' }},
                messaging = {{ subscriptions = {{ {{ subject = 'plugin.echo', queue = 'lua' }} }} }},
                on_message = function(message) {body} end,
            }}
        "#), "plugin.lua").unwrap()
    }

    #[tokio::test]
    async fn receives_binary_requests_replies_and_preserves_subscription_on_reload() {
        let broker = Broker::default();
        let mut replies = broker.subscribe("reply", None).unwrap();
        let config = config(
            "assert(calls == 0); calls = calls + 1; assert(message.subject == 'plugin.echo'); rift.publish(message.reply, message.payload)",
        );
        let handler = MessageHandler::new(&config, broker.clone())
            .unwrap()
            .unwrap();
        let updates = handler.script_updates();
        // The subscription exists before the worker is scheduled.
        broker
            .publish_with_reply(
                "plugin.echo",
                Some("reply"),
                Bytes::from_static(b"\0\xffhello"),
            )
            .unwrap();
        let worker = tokio::spawn(handler.run());
        for index in 0..2 {
            if index != 0 {
                broker
                    .publish_with_reply(
                        "plugin.echo",
                        Some("reply"),
                        Bytes::from_static(b"\0\xffhello"),
                    )
                    .unwrap();
            }
            let reply = timeout(Duration::from_secs(2), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&reply.payload[..], b"\0\xffhello");
        }
        updates.send_replace(
            super::tests::config("rift.publish(message.reply, 'updated')")
                .on_message
                .unwrap(),
        );
        broker
            .publish_with_reply("plugin.echo", Some("reply"), Bytes::new())
            .unwrap();
        assert_eq!(
            &timeout(Duration::from_secs(2), replies.recv())
                .await
                .unwrap()
                .unwrap()
                .payload[..],
            b"updated"
        );
        worker.abort();
        let _ = worker.await;
    }

    #[tokio::test]
    async fn callback_error_does_not_stop_later_messages() {
        let broker = Broker::default();
        let mut replies = broker.subscribe("reply", None).unwrap();
        let handler = MessageHandler::new(&config("if message.payload == 'bad' then error('bad input') end; rift.publish('reply', message.payload)"), broker.clone()).unwrap().unwrap();
        let worker = tokio::spawn(handler.run());
        broker
            .publish("plugin.echo", Bytes::from_static(b"bad"))
            .unwrap();
        broker
            .publish("plugin.echo", Bytes::from_static(b"good"))
            .unwrap();
        assert_eq!(
            &timeout(Duration::from_secs(2), replies.recv())
                .await
                .unwrap()
                .unwrap()
                .payload[..],
            b"good"
        );
        worker.abort();
        let _ = worker.await;
    }

    #[tokio::test]
    async fn overflowing_lua_queue_recovers_for_later_deliveries() {
        let broker = Broker::new(crate::messaging::BrokerConfig {
            subscription_capacity: 1,
            ..Default::default()
        })
        .unwrap();
        let mut replies = broker.subscribe("reply", None).unwrap();
        let handler = MessageHandler::new(
            &config("rift.publish('reply', message.payload)"),
            broker.clone(),
        )
        .unwrap()
        .unwrap();
        broker
            .publish("plugin.echo", Bytes::from_static(b"discarded"))
            .unwrap();
        assert_eq!(
            broker
                .publish("plugin.echo", Bytes::from_static(b"overflow"))
                .unwrap()
                .slow_consumers,
            1
        );
        let worker = tokio::spawn(handler.run());
        timeout(Duration::from_secs(2), async {
            loop {
                tokio::time::sleep(Duration::from_millis(5)).await;
                if broker
                    .publish("plugin.echo", Bytes::from_static(b"recovered"))
                    .unwrap()
                    .delivered
                    == 1
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let reply = timeout(Duration::from_secs(2), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply.payload[..], b"recovered");
        worker.abort();
        let _ = worker.await;
    }
}
