//! Terminal mode (ADR 0004): processes on pseudo-terminals the daemon
//! manages, for the cockpit's embedded terminal and `aux attach`.
//!
//! A `harness-tui` terminal opens a session in its harness's own TUI,
//! resuming the same vendor session (`claude --resume <id>`, ...). The
//! session is handed over first ([`StepExecutor::take_over`]): the turn in
//! progress ends, the ACP adapter lets go of the session, and turns for it
//! wait. When the TUI exits, the session is given back and reopened over ACP
//! with `session/load`, so the agent continues with everything said in the
//! TUI. When the harness cannot do that, the terminal is a shell in the
//! run's worktree instead, with a banner saying why.
//!
//! Output is kept in a scrollback (the last [`SCROLLBACK`] bytes) and
//! broadcast to the connections attached to the terminal; attaching replays
//! the scrollback first, without gaps.
//!
//! [`StepExecutor::take_over`]: crate::executor::StepExecutor::take_over

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::Duration;

use agentux_api::rpc::{
    ListTerminals, OpenTerminal, ResizeTerminal, Terminal, TerminalCommand, TerminalState,
    WriteTerminal,
};
use agentux_api::{MessageFrom, Session, SessionEvent, SessionState};
use agentux_store::{new_id, now_ms};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, watch};

use crate::engine::{Engine, Error, RunHost};
use crate::executor::StepHost;

/// Output kept per terminal for clients that attach later.
pub(crate) const SCROLLBACK: usize = 256 * 1024;
/// Output chunks buffered per attached client.
const CHANNEL: usize = 4096;
/// After `terminals.close`, how long the process gets to exit on SIGHUP
/// before its process group is killed.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// After the process exits, how long its last output may take to drain.
const DRAIN: Duration = Duration::from_secs(2);

/// What a terminal sends its clients.
#[derive(Debug, Clone)]
pub(crate) enum Chunk {
    Output(Arc<[u8]>),
    /// The last chunk.
    Exit(Option<i32>),
}

/// Every terminal of the daemon.
#[derive(Default)]
pub(crate) struct Terminals {
    map: Mutex<HashMap<String, Arc<Term>>>,
}

impl Terminals {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Arc<Term>>> {
        lock(&self.map)
    }

    pub(crate) fn get(&self, id: &str) -> Result<Arc<Term>, Error> {
        self.map()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no terminal {id}")))
    }

    fn insert(&self, term: Arc<Term>) {
        let id = term.info().terminal_id;
        self.map().insert(id, term);
    }

    fn remove(&self, id: &str) {
        self.map().remove(id);
    }

    /// Closes the terminals of a run that ended.
    pub(crate) fn close_run(&self, run_id: &str) {
        let terms: Vec<Arc<Term>> = self
            .map()
            .values()
            .filter(|t| t.info().run_id.as_deref() == Some(run_id))
            .cloned()
            .collect();
        for term in terms {
            term.close();
        }
    }
}

/// The pseudo-terminal of a running process.
struct Pty {
    master: Box<dyn MasterPty + Send>,
    /// To the thread that writes the terminal's input, in order.
    input: mpsc::Sender<Vec<u8>>,
    /// The process (a session leader, so also its process group).
    pid: Option<u32>,
}

#[derive(Default)]
struct Scroll {
    bytes: VecDeque<u8>,
    /// Set once the terminal has exited.
    exit: Option<Option<i32>>,
}

/// One terminal.
pub(crate) struct Term {
    info: Mutex<Terminal>,
    /// Held while a chunk is sent, so that [`Term::subscribe`] gets the
    /// scrollback and the live chunks without gap or overlap.
    scroll: Mutex<Scroll>,
    chunks: broadcast::Sender<Chunk>,
    pty: Mutex<Option<Pty>>,
    /// Set by [`Term::close`].
    closed: watch::Sender<bool>,
    /// Set by [`Term::finish`].
    exited: watch::Sender<bool>,
    forget: Arc<Forget>,
}

/// Removes a terminal from the registry once it exits.
type Forget = dyn Fn(&str) + Send + Sync;

impl Term {
    pub(crate) fn info(&self) -> Terminal {
        lock(&self.info).clone()
    }

    /// The scrollback, whether the terminal has exited, and a receiver for
    /// what comes next.
    pub(crate) fn subscribe(&self) -> (Vec<u8>, Option<Option<i32>>, broadcast::Receiver<Chunk>) {
        let scroll = lock(&self.scroll);
        let bytes = scroll.bytes.iter().copied().collect();
        (bytes, scroll.exit, self.chunks.subscribe())
    }

    fn output(&self, bytes: &[u8]) {
        let mut scroll = lock(&self.scroll);
        if scroll.exit.is_some() || bytes.is_empty() {
            return;
        }
        scroll.bytes.extend(bytes);
        let excess = scroll.bytes.len().saturating_sub(SCROLLBACK);
        scroll.bytes.drain(..excess);
        let _ = self.chunks.send(Chunk::Output(bytes.into()));
    }

    /// Marks the terminal exited and tells its clients. Only the first call
    /// counts.
    fn finish(&self, code: Option<i32>) {
        {
            let mut scroll = lock(&self.scroll);
            if scroll.exit.is_some() {
                return;
            }
            scroll.exit = Some(code);
            let _ = self.chunks.send(Chunk::Exit(code));
        }
        // Closes the master: whatever still holds the terminal gets SIGHUP.
        lock(&self.pty).take();
        let id = {
            let mut info = lock(&self.info);
            info.state = TerminalState::Exited;
            info.exit_code = code;
            info.terminal_id.clone()
        };
        self.exited.send_replace(true);
        (self.forget)(&id);
    }

    pub(crate) fn write(&self, data: &[u8]) -> Result<(), Error> {
        if self.is_exited() {
            return Err(Error::Conflict("the terminal has exited".into()));
        }
        // While a terminal waits for its TUI to start, input is dropped.
        if let Some(pty) = lock(&self.pty).as_ref() {
            let _ = pty.input.send(data.to_vec());
        }
        Ok(())
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        check_size(cols, rows)?;
        if self.is_exited() {
            return Err(Error::Conflict("the terminal has exited".into()));
        }
        if let Some(pty) = lock(&self.pty).as_ref() {
            pty.master
                .resize(size(cols, rows))
                .map_err(|e| Error::Internal(format!("cannot resize the terminal: {e}")))?;
        }
        let mut info = lock(&self.info);
        info.cols = cols;
        info.rows = rows;
        Ok(())
    }

    /// Ends the terminal: SIGHUP to its process (by closing the terminal),
    /// then SIGKILL to its process group if it is still there after
    /// [`CLOSE_GRACE`]. A terminal still waiting to start just exits.
    pub(crate) fn close(self: &Arc<Self>) {
        self.closed.send_replace(true);
        let pid = match lock(&self.pty).take() {
            Some(pty) => pty.pid,
            None => {
                self.finish(None);
                return;
            }
        };
        let term = Arc::clone(self);
        let mut exited = self.exited.subscribe();
        tokio::spawn(async move {
            let gone = tokio::time::timeout(CLOSE_GRACE, exited.wait_for(|e| *e)).await;
            if gone.is_err() {
                if let Some(group) = pid
                    .and_then(|p| i32::try_from(p).ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    let _ =
                        rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                }
                term.finish(None);
            }
        });
    }

    fn is_exited(&self) -> bool {
        lock(&self.scroll).exit.is_some()
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Starts `argv` on a new pseudo-terminal of the terminal's size.
    fn spawn(
        self: &Arc<Self>,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<(), String> {
        let (program, args) = argv.split_first().ok_or("nothing to run")?;
        let (cols, rows) = {
            let info = lock(&self.info);
            (info.cols, info.rows)
        };
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .map_err(|e| format!("cannot open a pseudo-terminal: {e}"))?;
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(cwd);
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("cannot start `{program}`: {e}"))?;
        // Only the child keeps the terminal's slave side open, so reads end
        // once it (and anything it left behind) is gone.
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("cannot read the terminal: {e}"))?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("cannot write to the terminal: {e}"))?;
        let (input, inputs) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for bytes in inputs {
                if writer
                    .write_all(&bytes)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        });
        *lock(&self.pty) = Some(Pty {
            master: pair.master,
            input,
            pid: child.process_id(),
        });
        {
            let mut info = lock(&self.info);
            info.argv = argv.to_vec();
            info.state = TerminalState::Running;
        }

        let (drained, drain) = mpsc::channel::<()>();
        let term = Arc::clone(self);
        std::thread::spawn(move || {
            let mut buf = vec![0; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => term.output(&buf[..n]),
                }
            }
            let _ = drained.send(());
        });
        let term = Arc::clone(self);
        std::thread::spawn(move || {
            let status = child.wait();
            let _ = drain.recv_timeout(DRAIN);
            let code = status
                .ok()
                .filter(|s| s.signal().is_none())
                .map(|s| i32::try_from(s.exit_code()).unwrap_or(i32::MAX));
            term.finish(code);
        });
        // Closed while starting.
        if self.is_closed() {
            self.close();
        }
        Ok(())
    }
}

impl Engine {
    /// `terminals.open`. Returns at once: a `harness-tui` terminal of a live
    /// session is `waiting` until the session's turn in progress ends.
    pub(crate) fn open_terminal(&self, params: OpenTerminal) -> Result<Arc<Term>, Error> {
        check_size(params.cols, params.rows)?;
        let target = self.terminal_target(&params)?;
        let command = params.command.unwrap_or(if target.session.is_some() {
            TerminalCommand::HarnessTui
        } else {
            TerminalCommand::Shell
        });
        if command == TerminalCommand::HarnessTui && target.session.is_none() {
            return Err(Error::InvalidParams(
                "`harness-tui` needs a sessionId".into(),
            ));
        }
        if !target.cwd.is_dir() {
            return Err(Error::Conflict(format!(
                "the worktree {} does not exist",
                target.cwd.display()
            )));
        }
        let terminals = Arc::downgrade(&self.inner);
        let term = Arc::new(Term {
            info: Mutex::new(Terminal {
                terminal_id: new_id(),
                session_id: target.session.as_ref().map(|s| s.id.clone()),
                run_id: Some(target.run_id.clone()),
                command,
                fallback: None,
                argv: Vec::new(),
                cwd: target.cwd.to_string_lossy().into_owned(),
                cols: params.cols,
                rows: params.rows,
                state: TerminalState::Waiting,
                exit_code: None,
                created_at: now_ms(),
            }),
            scroll: Mutex::default(),
            chunks: broadcast::channel(CHANNEL).0,
            pty: Mutex::new(None),
            closed: watch::channel(false).0,
            exited: watch::channel(false).0,
            forget: Arc::new(move |id: &str| {
                if let Some(inner) = terminals.upgrade() {
                    inner.terminals.remove(id);
                }
            }),
        });
        self.inner.terminals.insert(Arc::clone(&term));

        let env = target.env();
        match (command, &target.session) {
            (TerminalCommand::HarnessTui, Some(session)) => {
                let argv = session
                    .vendor_session_id
                    .as_deref()
                    .and_then(|id| self.inner.settings.tui.resume(&session.harness, id));
                match argv {
                    None => {
                        let reason = match &session.vendor_session_id {
                            None => format!(
                                "The {} session has no vendor session id yet, so its TUI cannot resume it.",
                                session.harness
                            ),
                            Some(_) => format!(
                                "{} cannot resume a session by id in its own TUI.",
                                session.harness
                            ),
                        };
                        fallback(&term, &target, &env, &reason)?;
                    }
                    Some(argv) if target.live => {
                        let engine = self.clone();
                        let term = Arc::clone(&term);
                        tokio::spawn(async move {
                            engine.hand_over(term, target, argv, env).await;
                        });
                    }
                    // No ACP session holds it (the session or its run
                    // ended): the TUI can just resume it.
                    Some(argv) => {
                        term.output(
                            format!(
                                "[agentux] Opening the ended {} session in {}'s TUI.\r\n",
                                session.role, session.harness
                            )
                            .as_bytes(),
                        );
                        if let Err(e) = term.spawn(&argv, &target.cwd, &env) {
                            fallback(&term, &target, &env, &format!("{e}."))?;
                        }
                    }
                }
            }
            _ => {
                let argv = [shell()];
                term.spawn(&argv, &target.cwd, &env).map_err(|e| {
                    term.close();
                    Error::Internal(e)
                })?;
            }
        }
        Ok(term)
    }

    /// Hands a live session over to its TUI for as long as the terminal
    /// runs, then gives it back.
    async fn hand_over(
        &self,
        term: Arc<Term>,
        target: Target,
        argv: Vec<String>,
        env: Vec<(String, String)>,
    ) {
        let Some(session) = target.session.clone() else {
            return;
        };
        let host: Arc<dyn StepHost> = Arc::new(target.host(self));
        term.output(
            format!(
                "[agentux] Handing the {} session over to {}'s TUI; it starts once the agent's current turn ends.\r\n",
                session.role, session.harness
            )
            .as_bytes(),
        );
        let executor = Arc::clone(&self.inner.executor);
        let mut closed = term.closed.subscribe();
        let held = tokio::select! {
            held = executor.take_over(&target.run_id, &session.id, Arc::clone(&host)) => held,
            _ = closed.wait_for(|c| *c) => return,
        };
        let held = match held {
            Ok(held) => held,
            Err(reason) => {
                let reason = format!("The session cannot be handed over to its TUI: {reason}.");
                if let Err(e) = fallback(&term, &target, &env, &reason) {
                    term.output(format!("[agentux] {e}\r\n").as_bytes());
                    term.finish(None);
                }
                return;
            }
        };
        if term.is_closed() {
            executor.give_back(held, host).await;
            return;
        }
        let terminal_id = term.info().terminal_id;
        host.session_event(
            &session.id,
            SessionEvent::Message {
                from: MessageFrom::System,
                text: format!(
                    "Opened in {}'s TUI (terminal {terminal_id}); turns for this session wait until it is closed.",
                    session.harness
                ),
            },
        );
        let mut env = env;
        env.push((
            "AGENTUX_VENDOR_SESSION_ID".into(),
            held.vendor_session_id.clone(),
        ));
        if let Err(e) = term.spawn(&argv, &target.cwd, &env) {
            executor.give_back(held, Arc::clone(&host)).await;
            let reason = format!("{e}.");
            if let Err(e) = fallback(&term, &target, &env, &reason) {
                term.output(format!("[agentux] {e}\r\n").as_bytes());
                term.finish(None);
            }
            return;
        }
        let mut exited = term.exited.subscribe();
        let _ = exited.wait_for(|e| *e).await;
        executor.give_back(held, Arc::clone(&host)).await;
        host.session_event(
            &session.id,
            SessionEvent::Message {
                from: MessageFrom::System,
                text: format!(
                    "Back from {}'s TUI (terminal {terminal_id}); the session continues over ACP.",
                    session.harness
                ),
            },
        );
    }

    /// The session or run a terminal is for.
    fn terminal_target(&self, params: &OpenTerminal) -> Result<Target, Error> {
        self.inner.store.read(|tx| {
            let session = match &params.session_id {
                Some(id) => Some(
                    tx.session(id)?
                        .ok_or_else(|| Error::NotFound(format!("no session {id}")))?,
                ),
                None => None,
            };
            let run_id = match (&session, &params.run_id) {
                (Some(s), Some(r)) if &s.run_id != r => {
                    return Err(Error::InvalidParams(format!(
                        "session {} is not in run {r}",
                        s.id
                    )));
                }
                (Some(s), _) => s.run_id.clone(),
                (None, Some(r)) => r.clone(),
                (None, None) => {
                    return Err(Error::InvalidParams("give a sessionId or a runId".into()));
                }
            };
            let record = tx
                .run(&run_id)?
                .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))?;
            let cwd = match (&session, &record.run.worktree) {
                (Some(s), _) => PathBuf::from(&s.cwd),
                (None, Some(w)) => PathBuf::from(w),
                (None, None) => {
                    return Err(Error::Conflict(format!("run {run_id} has no worktree yet")));
                }
            };
            let live = session
                .as_ref()
                .is_some_and(|s| s.state != SessionState::Ended)
                && !record.run.status.is_terminal();
            Ok(Target {
                run_id,
                project_id: record.run.project_id.clone(),
                step_index: record.run.step_index,
                step: record.run.step,
                session,
                cwd,
                live,
            })
        })
    }

    pub(crate) fn terminal(&self, id: &str) -> Result<Arc<Term>, Error> {
        self.inner.terminals.get(id)
    }

    pub(crate) fn write_terminal(&self, params: &WriteTerminal) -> Result<(), Error> {
        let data = agentux_api::decode_bytes(&params.data).map_err(Error::InvalidParams)?;
        self.terminal(&params.terminal_id)?.write(&data)
    }

    pub(crate) fn resize_terminal(&self, params: &ResizeTerminal) -> Result<(), Error> {
        self.terminal(&params.terminal_id)?
            .resize(params.cols, params.rows)
    }

    pub(crate) fn close_terminal(&self, id: &str) -> Result<(), Error> {
        self.terminal(id)?.close();
        Ok(())
    }

    pub(crate) fn terminals(&self, params: &ListTerminals) -> Vec<Terminal> {
        let mut list: Vec<Terminal> = self
            .inner
            .terminals
            .map()
            .values()
            .map(|t| t.info())
            .filter(|t| params.session_id.is_none() || t.session_id == params.session_id)
            .filter(|t| params.run_id.is_none() || t.run_id == params.run_id)
            .collect();
        list.sort_by(|a, b| (a.created_at, &a.terminal_id).cmp(&(b.created_at, &b.terminal_id)));
        list
    }
}

/// What a terminal opens on.
#[derive(Clone)]
struct Target {
    run_id: String,
    project_id: String,
    step_index: usize,
    step: agentux_api::StepKind,
    session: Option<Session>,
    cwd: PathBuf,
    /// The session is live: its ACP side must hand it over first.
    live: bool,
}

impl Target {
    fn host(&self, engine: &Engine) -> RunHost {
        RunHost {
            engine: engine.clone(),
            run_id: self.run_id.clone(),
            project_id: self.project_id.clone(),
            role: self
                .session
                .as_ref()
                .map(|s| s.role.clone())
                .unwrap_or_default(),
            step_index: self.step_index,
            step: self.step,
            cwd: self.cwd.to_string_lossy().into_owned(),
        }
    }

    /// The environment of the terminal's process, besides the daemon's own.
    fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("AGENTUX_RUN_ID".into(), self.run_id.clone()),
            (
                "AGENTUX_WORKTREE".into(),
                self.cwd.to_string_lossy().into_owned(),
            ),
        ];
        if let Some(session) = &self.session {
            env.push(("AGENTUX_SESSION_ID".into(), session.id.clone()));
            env.push(("AGENTUX_ROLE".into(), session.role.clone()));
            env.push(("AGENTUX_HARNESS".into(), session.harness.clone()));
            if let Some(id) = &session.vendor_session_id {
                env.push(("AGENTUX_VENDOR_SESSION_ID".into(), id.clone()));
            }
        }
        env
    }
}

/// Runs a shell instead of the TUI, after a banner saying why.
fn fallback(
    term: &Arc<Term>,
    target: &Target,
    env: &[(String, String)],
    reason: &str,
) -> Result<(), Error> {
    let what = match &target.session {
        Some(s) => format!(
            "The {} session ({}) stays with agentuxd.",
            s.role, s.harness
        ),
        None => String::new(),
    };
    let banner = format!(
        "\r\n[agentux] {reason}\r\n[agentux] This is a shell in the run's worktree, {}. {what}\r\n\r\n",
        target.cwd.display()
    );
    {
        let mut info = lock(&term.info);
        info.command = TerminalCommand::Shell;
        info.fallback = Some(reason.to_string());
    }
    term.output(banner.as_bytes());
    let argv = [shell()];
    term.spawn(&argv, &target.cwd, env).map_err(|e| {
        term.finish(None);
        Error::Internal(e)
    })
}

/// `$SHELL`, else `/bin/sh`.
fn shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

fn check_size(cols: u16, rows: u16) -> Result<(), Error> {
    if cols == 0 || rows == 0 {
        return Err(Error::InvalidParams(
            "cols and rows must be at least 1".into(),
        ));
    }
    Ok(())
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
