//! A channel's task on the runtime — its source's, its recorder's — and how
//! a thread halts it and waits for it.

use std::future::Future;
use std::sync::{Arc, Condvar, Mutex};

use tokio::runtime::Handle;
use tokio::sync::watch;

pub(super) struct Task {
    /// Set once, by [`Task::halt`]; the task watches it.
    halt: watch::Sender<bool>,
    /// Whether the task has ended — set as it goes, however it goes, which
    /// [`Task::wait`] waits for. True before [`Task::spawn`].
    done: Arc<Done>,
}

struct Done {
    done: Mutex<bool>,
    ended: Condvar,
}

/// Owned by the task: says it is done as it drops — when the task returns,
/// and when a runtime shutting down drops it unfinished.
struct Finishing(Arc<Done>);

impl Drop for Finishing {
    fn drop(&mut self) {
        *self.0.done.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.0.ended.notify_all();
    }
}

impl Task {
    /// One not started yet.
    pub(super) fn new() -> Self {
        Self {
            halt: watch::Sender::new(false),
            done: Arc::new(Done {
                done: Mutex::new(true),
                ended: Condvar::new(),
            }),
        }
    }

    /// Starts it on `runtime`: `work`, given what says it is halted.
    pub(super) fn spawn<F>(&self, runtime: &Handle, work: impl FnOnce(watch::Receiver<bool>) -> F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        *self.done.done.lock().unwrap_or_else(|p| p.into_inner()) = false;
        let finishing = Finishing(Arc::clone(&self.done));
        let work = work(self.halt.subscribe());
        runtime.spawn(async move {
            let _finishing = finishing;
            work.await;
        });
    }

    /// Tells it to stop, without waiting.
    pub(super) fn halt(&self) {
        self.halt.send_replace(true);
    }

    /// Halts it and waits for it to end. Blocks the thread: never call it
    /// on the runtime's.
    pub(super) fn wait(&self) {
        self.halt();
        let done = self.done.done.lock().unwrap_or_else(|p| p.into_inner());
        drop(
            self.done
                .ended
                .wait_while(done, |done| !*done)
                .unwrap_or_else(|p| p.into_inner()),
        );
    }
}

impl Drop for Task {
    /// Halts it only: a task's owner is dropped wherever its channel's last
    /// handle goes, a request on the runtime among them, which must not
    /// wait. The task ends by itself.
    fn drop(&mut self) {
        self.halt();
    }
}
