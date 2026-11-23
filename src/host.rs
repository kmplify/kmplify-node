//! Hooks for an application that embeds the provider.
//!
//! The headless binary is one host of [`crate::fabric_worker`]; an
//! application that ships the same worker inside its own window (the KMPLIFY
//! desktop app) is another. Both must run the SAME worker, or the two drift
//! and one of them speaks an older protocol than the other. What differs
//! between hosts is small and is all here:
//!
//! * **where the operator's choices come from** ([`ProviderHost::configure`]):
//!   the binary reads its environment and `settings.json`, an app reads its
//!   own settings screen, and either may change between two connections;
//! * **what the operator is told** ([`ProviderHost::log_line`],
//!   [`ProviderHost::workload_started`], [`ProviderHost::workload_ended`]);
//! * **what the host records about the work it provided**
//!   ([`ProviderHost::job_started`] and its [`JobObserver`],
//!   [`ProviderHost::hardware`]), for example an app's own usage history.
//!
//! Every method has a default that does nothing, so a host implements only
//! what it uses and a new hook is never a breaking change.
//!
//! # What a hook never receives
//!
//! A [`JobObserver`] sees the request and the engine's answer pass through,
//! because counting tokens needs them. It must count and drop: the provider
//! terms promise that a provider machine keeps no request or reply content
//! (DSGVO), and that promise covers whatever a host does with these
//! callbacks. Error codes handed to [`JobObserver::error`] are fixed machine
//! codes, never an upstream's message, which can echo the prompt.
//!
//! Hooks are called on the worker's own tasks. They must return quickly and
//! must not block: a slow hook delays the job or the session it reports on.
//!
//! # Installing
//!
//! ```no_run
//! use std::sync::Arc;
//! struct MyApp;
//! impl kmplify_node::host::ProviderHost for MyApp {}
//! kmplify_node::host::install(Arc::new(MyApp));
//! ```
//!
//! One host per process, like the worker itself. Installing again replaces
//! the previous host (useful in tests); a worker already running picks the
//! new one up at its next callback.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;

/// A job the worker was handed, as a host sees it: ids and the model name,
/// never the request.
#[derive(Clone, Debug)]
pub struct JobStart<'a> {
    /// The gateway's job id.
    pub id: &'a str,
    /// `chat` or `embeddings`.
    pub kind: &'a str,
    /// The model the consumer asked for.
    pub model: &'a str,
}

/// How a job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEnd {
    /// The job ran to its end, answered or failed (see the codes the
    /// observer was handed along the way).
    Finished,
    /// The job was aborted before its end: the consumer left (protocol v3.8
    /// `cancel`), the connection dropped, or the worker stopped.
    Cancelled,
}

/// Watches one job from its request to its end. Created by
/// [`ProviderHost::job_started`].
///
/// The calls arrive in order: [`request`](Self::request) once, then
/// [`http_status`](Self::http_status), then the body as
/// [`native_line`](Self::native_line) / [`openai_chunk`](Self::openai_chunk)
/// (streamed) or one [`reply`](Self::reply) (not streamed), with
/// [`error`](Self::error) wherever it failed, and [`end`](Self::end) exactly
/// once, last. A job that is aborted gets `end(JobEnd::Cancelled, ..)`
/// without the calls it did not reach.
pub trait JobObserver: Send {
    /// The request as the upstream will receive it (after envelope opening).
    /// `streamed` is whether the answer arrives in chunks.
    fn request(&mut self, _payload: &Value, _streamed: bool) {}
    /// The upstream answered with this HTTP status.
    fn http_status(&mut self, _status: u16) {}
    /// One line of an Ollama-native stream (`/api/chat` with `think`).
    fn native_line(&mut self, _line: &Value) {}
    /// One `data:` chunk of an OpenAI-shaped stream.
    fn openai_chunk(&mut self, _chunk: &Value) {}
    /// The whole body of an answer that was not streamed, before any
    /// reshaping.
    fn reply(&mut self, _body: &Value) {}
    /// The job failed. One of `unsupported_kind`, `stream_interrupted`,
    /// `timeout`, `upstream_unreachable`, `rate_limited`, `upstream_4xx`,
    /// `upstream_5xx`, `upstream_error`, or a refusal code of the worker
    /// itself (`peer_document_terms_missing`, an envelope refusal).
    fn error(&mut self, _code: &'static str) {}
    /// The job is over. `elapsed` is measured from the moment the worker
    /// took the job.
    fn end(&mut self, _how: JobEnd, _elapsed: Duration) {}
}

/// What an embedding application provides. See the module docs.
pub trait ProviderHost: Send + Sync {
    /// Called before every connection to the gateway, after the binary's own
    /// settings were applied: the place to apply the operator's current
    /// choices (sharing switches, marketplace listing, ceilings) so a change
    /// takes effect on the next reconnect without restarting the worker.
    fn configure(&self, _cfg: &mut crate::fabric_worker::WorkerConfig) {}

    /// The worker started with this node directory (identity, credentials,
    /// the provider terms acceptance). Called once per `run`.
    fn worker_started(&self, _node_dir: &Path) {}

    /// Every log line of the worker, without the `[fabric-worker]` prefix.
    /// The worker still prints it and keeps it in the status ring; this is
    /// for a host that has a log of its own.
    fn log_line(&self, _line: &str) {}

    /// A job arrived. Return an observer to watch it, or `None`.
    fn job_started(&self, _job: &JobStart<'_>) -> Option<Box<dyn JobObserver>> {
        None
    }

    /// A peer's container session was accepted on this machine (it is now
    /// pulling, then starting).
    fn workload_started(&self, _session: &str, _template: &str) {}

    /// A session this machine hosted ended. `outcome` is `stopped`.
    fn workload_ended(&self, _session: &str, _outcome: &str) {}

    /// The hardware this connection advertises: the hello's `gpu` block and
    /// its `cpu` block. Once per connection.
    fn hardware(&self, _gpu: &Value, _cpu: Option<&Value>) {}
}

fn cell() -> &'static RwLock<Option<Arc<dyn ProviderHost>>> {
    static HOST: RwLock<Option<Arc<dyn ProviderHost>>> = RwLock::new(None);
    &HOST
}

/// Install the host for this process, replacing any earlier one.
pub fn install(host: Arc<dyn ProviderHost>) {
    let mut g = cell().write().unwrap_or_else(|e| e.into_inner());
    *g = Some(host);
}

/// Remove the installed host. The worker then behaves exactly as the
/// headless binary does.
pub fn uninstall() {
    let mut g = cell().write().unwrap_or_else(|e| e.into_inner());
    *g = None;
}

/// The installed host, if any.
pub fn current() -> Option<Arc<dyn ProviderHost>> {
    cell().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Run `f` with the installed host. Nothing happens without one.
pub(crate) fn with(f: impl FnOnce(&dyn ProviderHost)) {
    if let Some(h) = current() {
        f(h.as_ref());
    }
}

/// One job's observer, ended exactly once.
///
/// Held by the job's task. When the task is aborted (a `cancel` frame, a
/// dropped connection) the guard is dropped without [`finish`](Self::finish)
/// and reports the job as cancelled, so an aborted job is still counted.
pub(crate) struct ObservedJob {
    observer: std::sync::Mutex<Option<Box<dyn JobObserver>>>,
    started: std::time::Instant,
}

impl ObservedJob {
    pub(crate) fn start(job: &JobStart<'_>) -> Arc<Self> {
        let observer = current().and_then(|h| h.job_started(job));
        Arc::new(ObservedJob {
            observer: std::sync::Mutex::new(observer),
            started: std::time::Instant::now(),
        })
    }

    /// An observer-less job, for tests that call the job runner directly.
    #[cfg(test)]
    pub(crate) fn none() -> Arc<Self> {
        Arc::new(ObservedJob {
            observer: std::sync::Mutex::new(None),
            started: std::time::Instant::now(),
        })
    }

    /// A job watched by `observer`, for tests that call the job runner
    /// directly.
    #[cfg(test)]
    pub(crate) fn watched(observer: Box<dyn JobObserver>) -> Arc<Self> {
        Arc::new(ObservedJob {
            observer: std::sync::Mutex::new(Some(observer)),
            started: std::time::Instant::now(),
        })
    }

    /// Hand the observer to `f`, if there is one and it has not ended.
    pub(crate) fn with(&self, f: impl FnOnce(&mut dyn JobObserver)) {
        let mut g = self.observer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(o) = g.as_mut() {
            f(o.as_mut());
        }
    }

    fn end(&self, how: JobEnd) {
        let taken = self
            .observer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut o) = taken {
            o.end(how, self.started.elapsed());
        }
    }

    /// The job ran to its end.
    pub(crate) fn finish(&self) {
        self.end(JobEnd::Finished);
    }
}

impl Drop for ObservedJob {
    fn drop(&mut self) {
        self.end(JobEnd::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Seen(Arc<Mutex<Vec<String>>>);

    impl JobObserver for Seen {
        fn http_status(&mut self, s: u16) {
            self.0.lock().unwrap().push(format!("status {s}"));
        }
        fn end(&mut self, how: JobEnd, _elapsed: Duration) {
            self.0.lock().unwrap().push(format!("end {how:?}"));
        }
    }

    fn observed(seen: &Arc<Mutex<Vec<String>>>) -> Arc<ObservedJob> {
        Arc::new(ObservedJob {
            observer: Mutex::new(Some(Box::new(Seen(seen.clone())))),
            started: std::time::Instant::now(),
        })
    }

    #[test]
    fn a_finished_job_ends_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let job = observed(&seen);
        job.with(|o| o.http_status(200));
        job.finish();
        drop(job);
        assert_eq!(*seen.lock().unwrap(), ["status 200", "end Finished"]);
    }

    #[test]
    fn a_dropped_job_is_cancelled() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        drop(observed(&seen));
        assert_eq!(*seen.lock().unwrap(), ["end Cancelled"]);
    }

    #[test]
    fn nothing_after_the_end() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let job = observed(&seen);
        job.finish();
        job.with(|o| o.http_status(500));
        drop(job);
        assert_eq!(*seen.lock().unwrap(), ["end Finished"]);
    }
}
