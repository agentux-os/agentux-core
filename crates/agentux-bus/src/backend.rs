//! What the bus needs from the daemon: the state of a run and a way to reach
//! the human. [`MemoryBackend`] implements it in memory for tests and for
//! `aux bus-stdio --standalone`.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, oneshot};

use crate::types::{HumanQuestion, RunId};

/// A boxed, sendable future, as returned by [`BusBackend`] methods.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Implemented by `agentuxd`: the workflow engine owns run state and the
/// cockpit inbox owns the human.
pub trait BusBackend: Send + Sync + 'static {
    /// The current state of a run, for `get_run_state`.
    fn run_state(&self, run: &RunId) -> BoxFuture<Result<RunState, BackendError>>;

    /// Puts a question in front of the human and resolves with the answer.
    /// It may take hours; the bus stops waiting after its configured time and
    /// delivers a late answer to the asker's mailbox.
    fn ask_human(&self, question: HumanQuestion) -> BoxFuture<Result<String, BackendError>>;
}

/// The part of a run's state the workflow engine reports to agents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RunState {
    /// The pipeline step the run is in, e.g. `implement`.
    pub step: Option<String>,
    /// Where the run stands, e.g. `running`, `waiting for approval`.
    pub status: Option<String>,
    /// The run's git branch, e.g. `aux/42`.
    pub branch: Option<String>,
    /// Results of the latest gate checks.
    pub checks: Vec<CheckResult>,
    /// Requests waiting on someone (approvals, reviews), in words.
    pub open_requests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckResult {
    /// The check's name in `agentux.yaml`, e.g. `test`.
    pub name: String,
    /// `None` while it has not run yet.
    pub passed: Option<bool>,
    /// Short output, e.g. the failing test names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

/// An in-memory [`BusBackend`]: run states are set by hand and questions wait
/// until [`MemoryBackend::answer`] is called.
#[derive(Default)]
pub struct MemoryBackend {
    inner: Mutex<MemoryState>,
    asked: Notify,
}

#[derive(Default)]
struct MemoryState {
    runs: HashMap<RunId, RunState>,
    questions: Vec<HumanQuestion>,
    waiting: HashMap<u64, oneshot::Sender<String>>,
}

impl MemoryBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_run_state(&self, run: &RunId, state: RunState) {
        self.lock().runs.insert(run.clone(), state);
    }

    /// Questions still waiting for an answer, oldest first.
    pub fn pending_questions(&self) -> Vec<HumanQuestion> {
        let state = self.lock();
        state
            .questions
            .iter()
            .filter(|q| state.waiting.contains_key(&q.id))
            .cloned()
            .collect()
    }

    /// Waits until a question is pending and returns the oldest one.
    pub async fn next_question(&self) -> HumanQuestion {
        loop {
            let asked = self.asked.notified();
            tokio::pin!(asked);
            asked.as_mut().enable();
            if let Some(question) = self.pending_questions().into_iter().next() {
                return question;
            }
            asked.await;
        }
    }

    /// Answers a pending question. Returns `false` if there is none with
    /// that id.
    pub fn answer(&self, question: u64, answer: impl Into<String>) -> bool {
        let sender = self.lock().waiting.remove(&question);
        sender.is_some_and(|tx| tx.send(answer.into()).is_ok())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.inner.lock().expect("memory backend lock")
    }
}

impl BusBackend for MemoryBackend {
    fn run_state(&self, run: &RunId) -> BoxFuture<Result<RunState, BackendError>> {
        let state = self.lock().runs.get(run).cloned().unwrap_or_default();
        Box::pin(async move { Ok(state) })
    }

    fn ask_human(&self, question: HumanQuestion) -> BoxFuture<Result<String, BackendError>> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.lock();
            state.waiting.insert(question.id, tx);
            state.questions.push(question);
        }
        self.asked.notify_waiters();
        Box::pin(async move {
            rx.await
                .map_err(|_| BackendError("the question was withdrawn".into()))
        })
    }
}
