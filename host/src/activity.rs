// Only host-generated metadata belongs here. Never pass guest payloads or errors.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub(crate) fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn bounded_name(name: &str) -> String {
    name.chars().take(128).collect()
}

/// Captures the caller's span now, not when a cached component is constructed.
pub(crate) struct Activity {
    span: tracing::Span,
    kind: &'static str,
    start: Instant,
    finished: bool,
}

impl Activity {
    pub(crate) fn new(span: tracing::Span, kind: &'static str) -> Self {
        tracing::info!(parent: &span, kind, "activity started");
        Self {
            span,
            kind,
            start: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn finish(mut self, outcome: &'static str, bytes: usize, status: Option<u16>) {
        self.finished = true;
        tracing::info!(parent: &self.span, kind = self.kind, outcome, bytes, status,
            elapsed_ms = self.start.elapsed().as_millis() as u64, "activity finished");
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if !self.finished {
            tracing::info!(parent: &self.span, kind = self.kind, outcome = "interrupted",
                elapsed_ms = self.start.elapsed().as_millis() as u64, "activity finished");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Capture {
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
        pub(crate) fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
            // Other tests hit the same callsites without a scoped subscriber.
            // Keep an unfiltered fallback installed so their first use cannot
            // cache Interest::never and suppress records in a capturing test.
            static INIT: std::sync::Once = std::sync::Once::new();
            INIT.call_once(|| {
                tracing::subscriber::set_global_default(tracing_subscriber::registry()).unwrap();
            });
            let writer = self.clone();
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish()
        }
    }

    #[test]
    fn lifecycle_records_outcomes_and_interruption_and_escapes_names() {
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.subscriber(), || {
            for outcome in ["ok", "error"] {
                Activity::new(tracing::info_span!("tool", tool = "weather"), "tool")
                    .finish(outcome, 12, None);
            }
            drop(Activity::new(
                tracing::info_span!("tool",
                tool = ?bounded_name(&format!("bad\nforged{}", "x".repeat(300)))),
                "tool",
            ));
        });
        let text = capture.text();
        assert!(text.contains("outcome=\"ok\""));
        assert!(text.contains("outcome=\"error\""));
        assert!(text.contains("outcome=\"interrupted\""));
        assert!(text.contains("bad\\nforged"));
        assert!(!text.contains("\nforged"));
        assert_eq!(bounded_name(&"é".repeat(200)).chars().count(), 128);
    }
}
