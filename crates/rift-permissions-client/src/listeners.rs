use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, PoisonError},
};

type Callback<E> = Arc<dyn Fn(&E) + Send + Sync>;

struct Registry<E> {
    next: u64,
    callbacks: Vec<(u64, Callback<E>)>,
}

/// A set of callbacks for events of type `E`, for implementations of
/// [`RiftPermissions::on_change`](crate::RiftPermissions::on_change).
pub struct Listeners<E> {
    registry: Arc<Mutex<Registry<E>>>,
}

impl<E> Default for Listeners<E> {
    fn default() -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry {
                next: 0,
                callbacks: Vec::new(),
            })),
        }
    }
}

impl<E: 'static> Listeners<E> {
    /// Register a callback until the returned [`Listener`] is dropped.
    pub fn add(&self, callback: impl Fn(&E) + Send + Sync + 'static) -> Listener {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let id = registry.next;
        registry.next += 1;
        registry.callbacks.push((id, Arc::new(callback)));
        let weak = Arc::downgrade(&self.registry);
        Listener(Some(Box::new(move || {
            if let Some(registry) = weak.upgrade() {
                registry
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .callbacks
                    .retain(|(other, _)| *other != id);
            }
        })))
    }

    /// Call every callback in registration order. A panicking callback is
    /// logged and does not stop the others.
    pub fn emit(&self, event: &E) {
        let callbacks: Vec<Callback<E>> = self
            .registry
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .callbacks
            .iter()
            .map(|(_, callback)| callback.clone())
            .collect();
        for callback in callbacks {
            if catch_unwind(AssertUnwindSafe(|| callback(event))).is_err() {
                log::error!("rift-permissions: a change listener panicked");
            }
        }
    }
}

/// A registered callback. Dropping it unregisters the callback; call
/// [`forget`](Self::forget) to keep it for the rest of the process.
#[must_use = "dropping a Listener unregisters it at once"]
pub struct Listener(Option<Box<dyn FnOnce() + Send + Sync>>);

impl Listener {
    /// Keep the callback registered for as long as its source lives.
    pub fn forget(mut self) {
        self.0 = None;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(remove) = self.0.take() {
            remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn dropping_a_listener_unregisters_it() {
        let listeners = Listeners::<u32>::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let kept = listeners.add(move |n| {
            counter.fetch_add(*n as usize, Ordering::Relaxed);
        });
        let dropped = listeners.add(|_| panic!("listener bug"));
        listeners.emit(&2);
        drop(dropped);
        kept.forget();
        listeners.emit(&3);
        assert_eq!(calls.load(Ordering::Relaxed), 5);
    }
}
