//! A channel's task on the runtime — its source's, its recorder's — and how
//! it is halted and waited for.

use std::future::Future;
use std::sync::Mutex;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub(super) struct Task {
    /// Set once, by [`Task::halt`]; the task watches it.
    halt: watch::Sender<bool>,
    /// The task, until [`Task::wait`] has seen it end.
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Task {
    /// One not started yet.
    pub(super) fn new() -> Self {
        Self {
            halt: watch::Sender::new(false),
            handle: Mutex::new(None),
        }
    }

    /// Starts it on `runtime`: `work`, given what says it is halted.
    pub(super) fn spawn<F>(&self, runtime: &Handle, work: impl FnOnce(watch::Receiver<bool>) -> F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = runtime.spawn(work(self.halt.subscribe()));
        *self.handle.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
    }

    /// Tells it to stop, without waiting.
    pub(super) fn halt(&self) {
        self.halt.send_replace(true);
    }

    /// Halts it and waits for it to end. A runtime shutting down ends it
    /// too, unfinished.
    pub(super) async fn wait(&self) {
        self.halt();
        let handle = self.handle.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for Task {
    /// Halts it only: the task ends by itself.
    fn drop(&mut self) {
        self.halt();
    }
}
