//! The seam between the run state machine and whatever does the work of a
//! step that needs an agent (or a forge). The ACP-backed implementation comes
//! with the harness adapters; until then the daemon ships [`Unavailable`],
//! and tests (and `--fake-agents`) use [`FakeExecutor`].

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use agentux_api::{PullRequest, StepKind};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything an agent step needs to know.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTask {
    pub run_id: String,
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
    pub worktree: PathBuf,
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
    pub issue: Option<u64>,
}

/// Executes the steps that are not plain commands. Implementations must be
/// safe to call again for the same step after a crash: a daemon that stops
/// mid-step runs the step again on restart (ADR 0003). For pull requests that
/// means reusing an existing one for the branch.
pub trait StepExecutor: Send + Sync + 'static {
    fn run_agent<'a>(&'a self, task: &'a AgentTask) -> BoxFuture<'a, Result<AgentOutcome, String>>;

    fn open_pull_request<'a>(
        &'a self,
        task: &'a PullRequestTask,
    ) -> BoxFuture<'a, Result<PullRequest, String>>;
}

/// The default until harness adapters exist: agent and pull request steps
/// fail with an explanation. Gates and approvals still work.
pub struct Unavailable;

const UNAVAILABLE: &str = "no harness adapter is available yet in this agentuxd build \
     (start the daemon with --fake-agents to exercise pipelines without agents)";

impl StepExecutor for Unavailable {
    fn run_agent<'a>(&'a self, _: &'a AgentTask) -> BoxFuture<'a, Result<AgentOutcome, String>> {
        Box::pin(async { Err(UNAVAILABLE.to_string()) })
    }

    fn open_pull_request<'a>(
        &'a self,
        _: &'a PullRequestTask,
    ) -> BoxFuture<'a, Result<PullRequest, String>> {
        Box::pin(async { Err(UNAVAILABLE.to_string()) })
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
    fn run_agent<'a>(&'a self, task: &'a AgentTask) -> BoxFuture<'a, Result<AgentOutcome, String>> {
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
    ) -> BoxFuture<'a, Result<PullRequest, String>> {
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
            Ok(PullRequest {
                number: number as u64,
                url: format!("https://forge.invalid/pulls/{number}"),
            })
        })
    }
}
