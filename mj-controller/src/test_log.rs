//! A tracing subscriber for tests that have to prove what was logged, and at
//! which level.

use std::sync::{Arc, Mutex};

/// Every event logged on a thread while this is its default subscriber, as
/// its level and its fields written out, message included.
///
/// Install it with `tracing::subscriber::set_default`. A single-threaded
/// Tokio runtime runs spawned tasks on the same thread, so their events are
/// recorded too.
#[derive(Clone, Default)]
pub(crate) struct CapturedLog(Arc<Mutex<Vec<(tracing::Level, String)>>>);

impl CapturedLog {
    /// Events at `level` or more severe.
    pub(crate) fn at_or_above(&self, level: tracing::Level) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|(logged, _)| *logged <= level)
            .map(|(_, text)| text)
            .collect()
    }

    /// Events at exactly `level`.
    pub(crate) fn at(&self, level: tracing::Level) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|(logged, _)| *logged == level)
            .map(|(_, text)| text)
            .collect()
    }

    pub(crate) fn events(&self) -> Vec<(tracing::Level, String)> {
        self.0.lock().unwrap().clone()
    }
}

impl tracing::Subscriber for CapturedLog {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                use std::fmt::Write;
                let _ = write!(self.0, " {}={value:?}", field.name());
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        self.0
            .lock()
            .unwrap()
            .push((*event.metadata().level(), fields.0));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}
