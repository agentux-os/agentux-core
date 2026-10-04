//! Commands that talk to `agentuxd` over its socket.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentux_api::rpc::{BusPost, StartRun, Subscribe};
use agentux_api::{
    BusEndpoint, BusMessage, BusMessageKind, Client, Event, EventBody, MessageFrom, RequestKind,
    RequestStatus, Run, RunStatus, SessionEvent, SessionState, ToolStatus,
};

use crate::Result;

pub struct Remote {
    socket: PathBuf,
}

impl Remote {
    pub fn new(socket: Option<PathBuf>) -> Result<Self> {
        let socket = match socket {
            Some(socket) => socket,
            None => agentux_api::default_socket_path().ok_or(
                "cannot find the agentuxd socket: XDG_RUNTIME_DIR is not set; \
                 pass --socket or set AGENTUX_SOCKET",
            )?,
        };
        Ok(Self { socket })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn connect(&self) -> Result<Client> {
        Ok(Client::connect(&self.socket).await?)
    }

    pub async fn run(
        &self,
        project_dir: &Path,
        prompt: Option<String>,
        issue: Option<u64>,
        title: Option<String>,
        watch: bool,
    ) -> Result {
        if prompt.is_none() && issue.is_none() {
            return Err("give the run a --prompt or an --issue".into());
        }
        let dir =
            fs::canonicalize(project_dir).map_err(|e| format!("{}: {e}", project_dir.display()))?;
        let mut client = self.connect().await?;
        let project = client.register_project(&dir.to_string_lossy()).await?;
        let run = client
            .start_run(&StartRun {
                project_id: project.id,
                title,
                prompt,
                issue,
            })
            .await?;
        println!("{}", run.id);
        if watch {
            self.watch(&run.id).await
        } else {
            eprintln!(
                "started run {} in {}; follow it with `aux watch {}`",
                run.id, project.name, run.id
            );
            Ok(())
        }
    }

    pub async fn ps(&self, all: bool) -> Result {
        let mut client = self.connect().await?;
        let projects: HashMap<String, String> = client
            .list_projects()
            .await?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect();
        let runs: Vec<Run> = client
            .list_runs()
            .await?
            .into_iter()
            .filter(|r| all || !r.status.is_terminal())
            .collect();
        if runs.is_empty() {
            println!("no {}runs", if all { "" } else { "active " });
        } else {
            println!(
                "{:<10} {:<10} {:<10} {:<13} {:<16} TITLE / ACTIVITY",
                "RUN", "PROJECT", "STATUS", "STEP", "LOOPS"
            );
            for run in &runs {
                let project = projects.get(&run.project_id).map_or("?", String::as_str);
                let loops = format!(
                    "gate {}/{} rev {}/{}",
                    run.gate_attempt,
                    run.gate_max_attempts,
                    run.review_round,
                    run.review_max_rounds
                );
                println!(
                    "{:<10} {:<10} {:<10} {:<13} {:<16} {} — {}",
                    run.id,
                    truncate(project, 10),
                    run.status.as_str(),
                    run.step.as_str(),
                    loops,
                    run.title,
                    run.activity
                );
            }
        }

        let pending = client.list_requests(true).await?;
        if !pending.is_empty() {
            println!("\nWAITING FOR YOU");
            for request in pending {
                println!(
                    "{:<10} run {:<10} {:<11} {}",
                    request.id,
                    request.run_id,
                    request.kind.as_str(),
                    request.title
                );
            }
            println!(
                "\napprove with `aux approve <id>`, deny with `aux deny <id>`; \
                 answer an agent's question with `aux answer <id> <text>`"
            );
        }
        Ok(())
    }

    pub async fn resolve(&self, request_id: &str, approve: bool, answer: Option<String>) -> Result {
        let mut client = self.connect().await?;
        let request = if approve {
            client.approve(request_id, answer).await?
        } else {
            client.deny(request_id, answer).await?
        };
        println!("{} {}: {}", request.id, request.status, request.title);
        Ok(())
    }

    /// Prints a run's bus log: messages between agents, wakes, questions to
    /// the human and their answers, refusals.
    pub async fn bus(&self, run_id: &str) -> Result {
        let messages = self.connect().await?.list_bus(run_id).await?;
        if messages.is_empty() {
            println!("no bus traffic in run {run_id}");
        }
        for message in &messages {
            println!("{}", bus_line(message));
            if message.body != message.subject {
                for line in message.body.lines() {
                    println!("    | {line}");
                }
            }
        }
        Ok(())
    }

    /// Posts a message on a run's bus as the human.
    pub async fn post(&self, run_id: &str, to: &str, reply_to: Option<u64>, text: &str) -> Result {
        let to = match to.parse::<agentux_bus::Target>()? {
            agentux_bus::Target::Session(id) => BusEndpoint::Session {
                session_id: id.0,
                role: String::new(),
                vendor: String::new(),
            },
            agentux_bus::Target::Role(role) => BusEndpoint::Role { role },
            agentux_bus::Target::Run => BusEndpoint::Run,
            agentux_bus::Target::Human => {
                return Err(
                    "you are the human: post to `role:<name>`, `session:<id>` or `run`".into(),
                );
            }
        };
        let posted = self
            .connect()
            .await?
            .post_bus(&BusPost {
                run_id: run_id.into(),
                to: Some(to),
                body: text.into(),
                subject: None,
                in_reply_to: reply_to,
            })
            .await?;
        let mut line = format!(
            "posted #{} (exchange {})",
            posted.message_id, posted.exchange
        );
        if !posted.delivered_to.is_empty() {
            line.push_str(&format!(
                " to session(s) {}",
                posted.delivered_to.join(", ")
            ));
        }
        if let Some(role) = posted.queued_for_role {
            line.push_str(&format!(
                "; queued for {role}, whose session is being started"
            ));
        }
        println!("{line}");
        Ok(())
    }

    /// Sends a message to a live session as the human.
    pub async fn say(&self, session_id: &str, text: &str) -> Result {
        let prompted = self
            .connect()
            .await?
            .prompt_session(session_id, text)
            .await?;
        let session = &prompted.session;
        if prompted.queued {
            println!(
                "queued for the {} ({}): it gets your message after its current turn",
                session.role, session.harness
            );
        } else {
            println!("sent to the {} ({})", session.role, session.harness);
        }
        Ok(())
    }

    pub async fn cancel(&self, run_id: &str) -> Result {
        let run = self.connect().await?.cancel_run(run_id).await?;
        println!("{} {}", run.id, run.status);
        Ok(())
    }

    /// Prints the run's history, then follows it until it finishes. Fails if
    /// the run does not end `done`.
    pub async fn watch(&self, run_id: &str) -> Result {
        let mut client = self.connect().await?;
        // Fails early with "no run" instead of waiting forever.
        let detail = client.get_run(run_id).await?;
        println!("{} — {}", detail.run.id, detail.run.title);
        let mut events = client
            .subscribe(&Subscribe {
                run_id: Some(run_id.into()),
                since: Some(0),
            })
            .await?;
        let mut printer = Printer::default();
        while let Some(event) = events.next().await? {
            if let Some(status) = printer.print(&event)
                && status.is_terminal()
            {
                return match status {
                    RunStatus::Done => Ok(()),
                    _ => Err(format!("run {run_id} {status}").into()),
                };
            }
        }
        Err("agentuxd closed the connection".into())
    }
}

/// Turns events into terse lines, skipping run snapshots that change nothing
/// visible.
#[derive(Default)]
struct Printer {
    last: Option<(RunStatus, String, String)>,
    /// Session id to (role, last state).
    sessions: HashMap<String, (String, SessionState)>,
    /// Tool call id to title.
    tools: HashMap<String, String>,
    /// Last cost printed per session.
    costs: HashMap<String, f64>,
}

impl Printer {
    /// Returns the run status when the event is a run snapshot.
    fn print(&mut self, event: &Event) -> Option<RunStatus> {
        match &event.body {
            EventBody::Run { run } => {
                let shown = (run.status, run.step.to_string(), run.activity.clone());
                if self.last.as_ref() != Some(&shown) {
                    println!("[{}] {:<12} {}", run.status, run.step, run.activity);
                    if let Some(error) = &run.error {
                        println!("  error: {error}");
                    }
                    if let Some(pr) = &run.pull_request
                        && run.status == RunStatus::Done
                    {
                        println!("  pull request #{}: {}", pr.number, pr.url);
                    }
                    self.last = Some(shown);
                }
                return Some(run.status);
            }
            EventBody::Request { request } => match request.status {
                RequestStatus::Pending if request.kind == RequestKind::Question => {
                    println!("  question for you: {}", request.title);
                    for line in request.detail.lines() {
                        println!("  | {line}");
                    }
                    println!(
                        "  -> aux answer {} <answer> / aux deny {}",
                        request.id, request.id
                    );
                }
                RequestStatus::Pending => {
                    println!("  approval needed: {}", request.title);
                    for line in request.detail.lines() {
                        println!("  | {line}");
                    }
                    println!("  -> aux approve {} / aux deny {}", request.id, request.id);
                }
                status => println!("  request {} {status}", request.id),
            },
            EventBody::Attempt { attempt } => {
                if !matches!(attempt.status, agentux_api::AttemptStatus::Running) {
                    println!("  {} attempt {}", attempt.step, attempt.status);
                }
            }
            EventBody::Log { text } => {
                for line in text.lines() {
                    println!("  {line}");
                }
            }
            EventBody::Session { session } => {
                let previous = self
                    .sessions
                    .insert(session.id.clone(), (session.role.clone(), session.state));
                match previous {
                    None => println!(
                        "  session {} started: {} ({}) in {}",
                        session.id, session.role, session.harness, session.cwd
                    ),
                    Some((_, state))
                        if state != SessionState::Ended && session.state == SessionState::Ended =>
                    {
                        println!("  session {} ({}) ended", session.id, session.role)
                    }
                    Some(_) => {}
                }
            }
            EventBody::SessionEvent { session_id, event } => {
                let role = self
                    .sessions
                    .get(session_id)
                    .map_or("agent", |(role, _)| role.as_str())
                    .to_string();
                self.session_event(session_id, &role, event);
            }
            EventBody::BusMessage { message } => println!("  {}", bus_line(message)),
            EventBody::Project { .. } => {}
        }
        None
    }

    fn session_event(&mut self, session_id: &str, role: &str, event: &SessionEvent) {
        match event {
            SessionEvent::Message {
                from: MessageFrom::User,
                ..
            } => println!("  {role} <- prompt"),
            SessionEvent::Message {
                from: MessageFrom::Human,
                text,
            } => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    println!("  {role} <- you | {line}");
                }
            }
            SessionEvent::Message { text, .. } => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    println!("  {role} | {line}");
                }
            }
            SessionEvent::ToolCall {
                tool_call_id,
                tool,
                title,
                status,
                ..
            } => {
                if let Some(title) = title {
                    self.tools.insert(tool_call_id.clone(), title.clone());
                }
                let name = self
                    .tools
                    .get(tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| tool_call_id.clone());
                if let Some(tool) = tool {
                    println!("  {role} [{tool}] {name}");
                } else if let Some(status @ (ToolStatus::Ok | ToolStatus::Error)) = status {
                    println!("  {role} [{status}] {name}");
                }
            }
            SessionEvent::Diff { path, .. } => println!("  {role} [diff] {path}"),
            SessionEvent::Plan { items } => {
                println!("  {role} [plan]");
                for item in items {
                    println!("  {role}   - [{}] {}", item.status, item.text);
                }
            }
            SessionEvent::Usage { usage } => {
                if let Some(cost) = usage.cost_usd
                    && self.costs.insert(session_id.to_string(), cost) != Some(cost)
                {
                    println!(
                        "  {role} [usage] ${cost:.2}, {}/{} tokens in context",
                        usage.used_tokens, usage.context_tokens
                    );
                }
            }
            // The request event itself is printed.
            SessionEvent::Permission { .. } => {}
        }
    }
}

/// One line for a bus log entry.
fn bus_line(message: &BusMessage) -> String {
    let (from, to) = (endpoint(&message.from), endpoint(&message.to));
    match message.kind {
        BusMessageKind::Message
        | BusMessageKind::ReviewRequest
        | BusMessageKind::Handoff
        | BusMessageKind::HumanAnswer => {
            let mut line = format!("[bus] {from} -> {to}");
            if let Some(id) = message.message_id {
                line.push_str(&format!(" #{id}"));
            }
            if message.kind != BusMessageKind::Message {
                line.push_str(&format!(" {}", message.kind));
            }
            if message.max_turns > 0 && message.turn > 0 {
                line.push_str(&format!(" (turn {}/{})", message.turn, message.max_turns));
            }
            if let Some(role) = &message.queued_for_role {
                line.push_str(&format!(" [queued for {role}]"));
            }
            format!("{line}: {}", message.subject)
        }
        BusMessageKind::Question => format!(
            "[bus] {from} asks the human: {}{}",
            message.subject,
            message
                .request_id
                .as_ref()
                .map_or_else(String::new, |id| format!(" (aux answer {id} <answer>)"))
        ),
        BusMessageKind::Answer => format!("[bus] the human answered {to}: {}", message.subject),
        BusMessageKind::Wake => format!("[bus] {}", message.subject),
        BusMessageKind::TurnLimit | BusMessageKind::ToolDenied => {
            format!("[bus] refused for {from}: {}", message.subject)
        }
        BusMessageKind::Joined | BusMessageKind::Left => format!("[bus] {}", message.subject),
    }
}

fn endpoint(endpoint: &BusEndpoint) -> String {
    match endpoint {
        BusEndpoint::Session {
            session_id, role, ..
        } => format!("{role}({session_id})"),
        BusEndpoint::Role { role } => format!("role:{role}"),
        BusEndpoint::Run => "run".into(),
        BusEndpoint::Human => "human".into(),
        BusEndpoint::Daemon => "agentuxd".into(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}
