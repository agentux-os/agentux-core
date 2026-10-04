//! The seam between the run state machine and whatever does the work of a
//! step that needs an agent (or a forge). The daemon uses
//! [`AcpExecutor`](crate::AcpExecutor) by default; tests and `--fake-agents`
//! use [`FakeExecutor`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentux_api::{PullRequest, SessionEvent, SessionState, StepKind};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything an agent step needs to know.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTask {
    pub run_id: String,
    pub title: String,
    pub step_index: usize,
    pub step: StepKind,
    pub role: String,
    pub harness: String,
    pub model: Option<String>,
    /// What the human asked for.
    pub prompt: Option<String>,
    pub issue: Option<u64>,
    /// The prompt of a `custom` step.
    pub instructions: Option<String>,
    /// Output of the most recent successful plan step.
    pub plan: Option<String>,
    /// Failure output of a gate, or a reviewer's comments, that sent the run
    /// back to this step.
    pub feedback: Option<String>,
    /// The step that produced `feedback` (`gate` or `review`).
    pub feedback_from: Option<StepKind>,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    /// The commit the run's branch started from.
    pub base_commit: Option<String>,
    /// 1 for the first execution of this step in the run, counting retries
    /// after a restart.
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    Done {
        summary: String,
    },
    /// Only meaningful for review steps.
    ChangesRequested {
        comments: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PullRequestTask {
    pub run_id: String,
    pub title: String,
    pub branch: String,
    pub worktree: PathBuf,
    pub draft: bool,
    pub prompt: Option<String>,
    pub issue: Option<u64>,
    /// Output of the most recent successful plan step.
    pub plan: Option<String>,
    /// The approving review's summary.
    pub review: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PullRequestOutcome {
    Opened(PullRequest),
    /// No pull request could be opened for a reason that is not an error
    /// (no GitHub remote, `gh` missing or logged out). The branch is kept.
    Skipped(String),
}

/// What the daemon offers an executor while one agent step runs: recording
/// sessions and what happens in them, and asking the human.
pub trait StepHost: Send + Sync {
    /// Records a new session for the step's role and returns its id.
    fn open_session(&self, harness: &str, model: Option<&str>) -> Result<String, String>;

    fn session_state(&self, session_id: &str, state: SessionState);

    fn session_event(&self, session_id: &str, event: SessionEvent);

    /// Asks whether the agent may run a tool call. Resolves to `true` when
    /// allowed. Dropping the future withdraws the question.
    fn ask_permission(
        &self,
        session_id: &str,
        title: String,
        detail: String,
    ) -> BoxFuture<'static, bool>;
}

/// A host that records nothing and denies every permission, for executors
/// called outside the engine.
pub struct NoHost;

impl StepHost for NoHost {
    fn open_session(&self, harness: &str, _: Option<&str>) -> Result<String, String> {
        Ok(format!("{harness}-session"))
    }

    fn session_state(&self, _: &str, _: SessionState) {}

    fn session_event(&self, _: &str, _: SessionEvent) {}

    fn ask_permission(&self, _: &str, _: String, _: String) -> BoxFuture<'static, bool> {
        Box::pin(async { false })
    }
}

/// Executes the steps that are not plain commands. Implementations must be
/// safe to call again for the same step after a crash: a daemon that stops
/// mid-step runs the step again on restart (ADR 0003). For pull requests that
/// means reusing an existing one for the branch.
pub trait StepExecutor: Send + Sync + 'static {
    fn run_agent<'a>(
        &'a self,
        task: &'a AgentTask,
        host: Arc<dyn StepHost>,
    ) -> BoxFuture<'a, Result<AgentOutcome, String>>;

    fn open_pull_request<'a>(
        &'a self,
        task: &'a PullRequestTask,
    ) -> BoxFuture<'a, Result<PullRequestOutcome, String>>;

    /// The run finished or was cancelled: stop whatever is kept for it, such
    /// as harness sessions.
    fn release<'a>(&'a self, _run_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

type Script = Box<dyn Fn(&AgentTask) -> Result<AgentOutcome, String> + Send + Sync>;

/// A scripted stand-in for agents. Every call is recorded. By default every
/// agent step succeeds, reviews approve, and pull requests get a fake URL.
pub struct FakeExecutor {
    script: Script,
    delay: Duration,
    calls: Mutex<Vec<AgentTask>>,
    pull_requests: Mutex<Vec<PullRequestTask>>,
}

impl Default for FakeExecutor {
    fn default() -> Self {
        Self::new(|task| {
            Ok(AgentOutcome::Done {
                summary: format!("fake {} by {} done", task.step, task.role),
            })
        })
    }
}

impl FakeExecutor {
    pub fn new(
        script: impl Fn(&AgentTask) -> Result<AgentOutcome, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            script: Box::new(script),
            delay: Duration::ZERO,
            calls: Mutex::new(Vec::new()),
            pull_requests: Mutex::new(Vec::new()),
        }
    }

    /// Waits this long before answering each call, to stand in for an agent
    /// that takes time (and to give tests a window to interrupt it).
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Agent calls so far, in order.
    pub fn calls(&self) -> Vec<AgentTask> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn pull_requests(&self) -> Vec<PullRequestTask> {
        self.pull_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl StepExecutor for FakeExecutor {
    fn run_agent<'a>(
        &'a self,
        task: &'a AgentTask,
        _host: Arc<dyn StepHost>,
    ) -> BoxFuture<'a, Result<AgentOutcome, String>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(task.clone());
            (self.script)(task)
        })
    }

    fn open_pull_request<'a>(
        &'a self,
        task: &'a PullRequestTask,
    ) -> BoxFuture<'a, Result<PullRequestOutcome, String>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            let mut prs = self.pull_requests.lock().unwrap_or_else(|e| e.into_inner());
            // Idempotent like a real forge client: one PR per branch.
            let number = match prs.iter().position(|p| p.branch == task.branch) {
                Some(i) => i + 1,
                None => {
                    prs.push(task.clone());
                    prs.len()
                }
            };
            Ok(PullRequestOutcome::Opened(PullRequest {
                number: number as u64,
                url: format!("https://forge.invalid/pulls/{number}"),
            }))
        })
    }
}
