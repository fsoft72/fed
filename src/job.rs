//! One-shot background jobs: run a closure on its own thread and poll for its result.
//!
//! Every job reports the same way: the value, or a `JobError` when the closure panicked or
//! the thread vanished, so a crash in background work is never silently lost. The egui
//! context given at spawn time is repainted when the result is ready.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

/// Why a background job produced no value.
#[derive(thiserror::Error, Clone, Debug, PartialEq, Eq)]
pub enum JobError {
    /// The job panicked with this message.
    #[error("background task crashed: {0}")]
    Panicked(String),
    /// The thread ended without sending a result (its result was already taken).
    #[error("background task stopped without a result")]
    Lost,
}

/// A closure running on a background thread, whose result is polled from the UI thread.
pub struct BackgroundJob<T> {
    /// Receives the result once, when the closure returns or panics.
    rx: Receiver<Result<T, JobError>>,
}

impl<T: Send + 'static> BackgroundJob<T> {
    /// Runs `work` on a new thread. When `ctx` is given it is repainted once the result is
    /// ready, so the next frame can `poll` it.
    pub fn spawn(ctx: Option<&egui::Context>, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.cloned();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .map_err(|payload| JobError::Panicked(panic_message(payload.as_ref())));
            // Nobody listening (the job was dropped) is fine.
            let _ = tx.send(result);
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
        BackgroundJob { rx }
    }
}

impl<T> BackgroundJob<T> {
    /// Returns the result once the job is done (`None` while it runs). The value is handed
    /// out once: polling again afterwards gives `None` until the thread has exited, then
    /// `JobError::Lost`, so drop the job after taking its result.
    pub fn poll(&self) -> Option<Result<T, JobError>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(JobError::Lost)),
        }
    }
}

/// Text of a panic payload (`panic!` with a literal or a formatted message).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "unknown panic".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Polls `job` until it is done (at most a few seconds).
    fn wait<T>(job: &BackgroundJob<T>) -> Result<T, JobError> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = job.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "the job never finished");
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_job_hands_out_its_value_once() {
        let job = BackgroundJob::spawn(None, || 6 * 7);
        assert_eq!(wait(&job), Ok(42));
        assert_eq!(
            wait(&job),
            Err(JobError::Lost),
            "the value is not handed out again"
        );
    }

    #[test]
    fn a_panic_becomes_an_error_with_its_message() {
        let job = BackgroundJob::<u8>::spawn(None, || panic!("disk on fire"));
        assert_eq!(wait(&job), Err(JobError::Panicked("disk on fire".into())));
        let n = 3;
        let job = BackgroundJob::<u8>::spawn(None, move || panic!("failed {n} times"));
        assert_eq!(wait(&job), Err(JobError::Panicked("failed 3 times".into())));
    }

    #[test]
    fn a_running_job_is_not_done() {
        let (tx, rx) = mpsc::channel::<()>();
        let job = BackgroundJob::spawn(None, move || rx.recv().is_ok());
        assert_eq!(job.poll(), None);
        tx.send(()).unwrap();
        assert_eq!(wait(&job), Ok(true));
    }
}
