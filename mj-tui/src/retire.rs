//! Frees replaced values away from the event loop.
//!
//! Replacing a session's transcript on the dashboard drops the previous one,
//! and that can be thousands of strings and rendered lines. Freeing them is
//! work the loop would otherwise do before it can handle the next key. The
//! loop instead hands the old value to one background thread, which costs a
//! channel send.

use std::any::Any;
use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

type Retired = Box<dyn Any + Send>;

fn dropper() -> Option<&'static Sender<Retired>> {
    static DROPPER: OnceLock<Option<Sender<Retired>>> = OnceLock::new();
    DROPPER
        .get_or_init(|| {
            let (sender, receiver) = channel::<Retired>();
            match std::thread::Builder::new()
                .name("mj-retire".into())
                .spawn(move || {
                    for value in receiver {
                        drop(value);
                    }
                }) {
                Ok(_) => Some(sender),
                Err(error) => {
                    tracing::warn!(%error, "could not start the retire thread; values are freed in place");
                    None
                }
            }
        })
        .as_ref()
}

/// Drops `value` on the retire thread. If that thread is gone (a destructor
/// panicked there) or never started, the value is dropped here instead and
/// the failure is logged.
pub(crate) fn retire<T: Send + 'static>(value: T) {
    let Some(dropper) = dropper() else {
        return;
    };
    if let Err(returned) = dropper.send(Box::new(value)) {
        tracing::warn!("the retire thread stopped; freeing in place");
        drop(returned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReportsItsThread(Sender<std::thread::ThreadId>);

    impl Drop for ReportsItsThread {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }

    #[test]
    fn a_retired_value_is_dropped_on_another_thread() {
        let (sender, receiver) = channel();
        retire(ReportsItsThread(sender));
        let dropped_on = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the retired value is dropped");
        assert_ne!(dropped_on, std::thread::current().id());
    }
}
