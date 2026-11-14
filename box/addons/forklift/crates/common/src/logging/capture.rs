//! Log capture for tests, here and in the crates that depend on this one.

use std::io;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

use super::SlogFormat;

thread_local! {
    static CAPTURE: std::cell::RefCell<Option<BufWriter>> = const { std::cell::RefCell::new(None) };
}

/// Writes an event into the capture buffer of the thread that emitted it,
/// and drops it when that thread captures nothing.
#[derive(Clone, Copy)]
struct ThreadWriter;

impl io::Write for ThreadWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        CAPTURE.with(|c| {
            if let Some(out) = &*c.borrow() {
                out.0.lock().expect("buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for ThreadWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

/// The JSON lines logged on this thread while it is alive.
pub struct Capture(BufWriter);

impl Capture {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.0.lock().expect("buffer").clone()).expect("utf-8")
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURE.with(|c| *c.borrow_mut() = None);
    }
}

/// Captures what this thread logs at info and above. It goes through one
/// global subscriber rather than a thread-local one: a thread-local
/// subscriber shares tracing's global callsite and level caches with every
/// other test, and another test logging the same lines at the same moment
/// can leave them cached as disabled, dropping events from the capture.
pub fn capture_logs() -> Capture {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ThreadWriter)
            .event_format(SlogFormat::Json)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global subscriber in the library tests");
    });
    let buf = BufWriter::default();
    CAPTURE.with(|c| *c.borrow_mut() = Some(buf.clone()));
    Capture(buf)
}

/// A `MakeWriter` that appends into a shared buffer, so a formatted event can
/// be read back in the test.
#[derive(Clone, Default)]
pub(crate) struct BufWriter(pub(crate) Arc<Mutex<Vec<u8>>>);

impl io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
