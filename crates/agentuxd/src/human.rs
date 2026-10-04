//! What the human does inside a run besides approving: typing into a live
//! session (`sessions.prompt`), posting on the run's bus (`bus.post`), and
//! loading a run's stored history in pages (`runs.events`).

use std::path::PathBuf;
use std::sync::Arc;

use agentux_api::rpc::{self, BusPost, BusPosted, Prompted, RunEvents};
use agentux_api::{BusEndpoint, EventBody, MessageFrom, SessionEvent, SessionState};
use agentux_bus::{BusError, PostMessage, RunId, SessionId, Target};

use crate::engine::{Engine, Error, RunHost};
use crate::executor::WakeTask;

impl Engine {
    /// `sessions.prompt`: sends the human's `text` to a live session as an
    /// extra turn, after the turn in progress (and any queued before it).
    /// The message is recorded right away as a `human` session event; a
    /// delivery failure is logged on the run.
    pub fn prompt_session(&self, session_id: &str, text: &str) -> Result<Prompted, Error> {
        if text.trim().is_empty() {
            return Err(Error::InvalidParams("the message is empty".into()));
        }
        let (session, record) = self.inner.store.write(|tx| {
            let session = tx
                .session(session_id)?
                .ok_or_else(|| Error::NotFound(format!("no session {session_id}")))?;
            let record = tx
                .run(&session.run_id)?
                .ok_or_else(|| Error::NotFound(format!("no run {}", session.run_id)))?;
            let ended = session.state == SessionState::Ended || record.run.status.is_terminal();
            if ended {
                return Err(Error::Conflict(format!(
                    "session {session_id} has ended; it cannot take messages"
                )));
            }
            tx.emit(
                Some(&session.run_id),
                EventBody::SessionEvent {
                    session_id: session_id.to_string(),
                    event: SessionEvent::Message {
                        from: MessageFrom::Human,
                        text: text.to_string(),
                    },
                },
            )?;
            Ok((session, record))
        })?;
        let queued = session.state != SessionState::Idle;
        let host = Arc::new(RunHost {
            engine: self.clone(),
            run_id: session.run_id.clone(),
            project_id: session.project_id.clone(),
            role: session.role.clone(),
            step_index: record.run.step_index,
            step: record.run.step,
            cwd: session.cwd.clone(),
        });
        let task = WakeTask {
            run_id: session.run_id.clone(),
            role: session.role.clone(),
            harness: session.harness.clone(),
            model: session.model.clone(),
            worktree: PathBuf::from(&session.cwd),
            session_id: Some(session.id.clone()),
            prompt: text.to_string(),
            human: true,
        };
        let engine = self.clone();
        tokio::spawn(async move {
            if let Err(e) = engine.inner.executor.wake(&task, host).await {
                let logged = engine.inner.store.write(|tx| {
                    tx.log(
                        &task.run_id,
                        format!("your message to the {} was not handled: {e}", task.role),
                    )
                });
                if let Err(e) = logged {
                    eprintln!(
                        "agentuxd: run {}: cannot record a failed message: {e}",
                        task.run_id
                    );
                }
            }
        });
        Ok(Prompted { session, queued })
    }

    /// `bus.post`: a message from the human on a run's bus, waking its
    /// recipients like an agent's message (a role without a session gets
    /// one started). The human is never cut off by turn limits.
    pub fn bus_post(&self, params: BusPost) -> Result<BusPosted, Error> {
        let BusPost {
            run_id,
            to,
            body,
            subject,
            in_reply_to,
        } = params;
        let body = match subject.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(subject) if subject.contains('\n') => {
                return Err(Error::InvalidParams("the subject must be one line".into()));
            }
            Some(subject) => format!("{subject}\n\n{body}"),
            None => body,
        };
        let to = match to {
            None => None,
            Some(BusEndpoint::Session { session_id, .. }) => {
                Some(Target::Session(SessionId(session_id)))
            }
            Some(BusEndpoint::Role { role }) => Some(Target::Role(role)),
            Some(BusEndpoint::Run) => Some(Target::Run),
            Some(BusEndpoint::Human | BusEndpoint::Daemon) => {
                return Err(Error::InvalidParams(
                    "the human can post to a session, a role or the run".into(),
                ));
            }
        };
        let status = self.inner.store.read(|tx| {
            tx.run(&run_id)?
                .map(|r| r.run.status)
                .ok_or_else(|| Error::NotFound(format!("no run {run_id}")))
        })?;
        if status.is_terminal() {
            return Err(Error::Conflict(format!(
                "run {run_id} is {status}; its bus is closed"
            )));
        }
        let bus = self.open_bus(&run_id)?;
        let posted = bus
            .post_from_human(
                &RunId(run_id),
                PostMessage {
                    to,
                    body,
                    in_reply_to,
                },
            )
            .map_err(bus_error)?;
        Ok(BusPosted {
            message_id: posted.message_id,
            exchange: posted.exchange,
            turn: posted.turn,
            delivered_to: posted.delivered_to.into_iter().map(|s| s.0).collect(),
            queued_for_role: posted.queued_for_role,
        })
    }

    /// `runs.events`: one page of a run's stored events, oldest first.
    pub fn run_events(&self, params: rpc::RunEventsParams) -> Result<RunEvents, Error> {
        let limit = params
            .limit
            .unwrap_or(rpc::RUN_EVENTS_DEFAULT_LIMIT)
            .clamp(1, rpc::RUN_EVENTS_MAX_LIMIT);
        self.inner.store.read(|tx| {
            if tx.run(&params.run_id)?.is_none() {
                return Err(Error::NotFound(format!("no run {}", params.run_id)));
            }
            let head_seq = tx.last_event_seq()?;
            let mut events = tx.events_page(
                params.since_seq.unwrap_or(0),
                Some(&params.run_id),
                Some(limit + 1),
            )?;
            let more = events.len() > limit as usize;
            events.truncate(limit as usize);
            Ok(RunEvents {
                events,
                more,
                head_seq,
            })
        })
    }
}

fn bus_error(e: BusError) -> Error {
    match e {
        BusError::UnknownRun { .. }
        | BusError::UnknownSession { .. }
        | BusError::SessionLeft { .. } => Error::Conflict(e.to_string()),
        BusError::Backend { .. } => Error::Internal(e.to_string()),
        _ => Error::InvalidParams(e.to_string()),
    }
}
