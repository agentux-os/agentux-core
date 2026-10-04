//! A minimal client: one request at a time over one connection.

use std::path::Path;
use std::{fmt, io};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::rpc::{self, method};
use crate::{Event, PermissionRequest, Project, Run, Session};

pub struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl Client {
    pub async fn connect(socket: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).await.map_err(|e| {
            ClientError::Io(io::Error::new(
                e.kind(),
                format!(
                    "cannot connect to agentuxd at {}: {e} (is the daemon running? try `aux daemon`)",
                    socket.display()
                ),
            ))
        })?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            lines: BufReader::new(reader).lines(),
            writer,
            next_id: 1,
        })
    }

    /// Sends one request and waits for its response.
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = rpc::Request {
            jsonrpc: rpc::VERSION.into(),
            id: Some(id.into()),
            method: method.into(),
            params: serde_json::to_value(params).map_err(protocol)?,
        };
        let mut line = serde_json::to_vec(&request).map_err(protocol)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;

        loop {
            let line =
                self.lines.next_line().await?.ok_or_else(|| {
                    ClientError::Protocol("the daemon closed the connection".into())
                })?;
            let value: Value = serde_json::from_str(&line).map_err(protocol)?;
            // Notifications have no id; they cannot arrive before a subscription.
            if value.get("id").is_none() {
                continue;
            }
            let response: rpc::Response = serde_json::from_value(value).map_err(protocol)?;
            if response.id != id {
                continue;
            }
            if let Some(error) = response.error {
                return Err(ClientError::Rpc(error));
            }
            return serde_json::from_value(response.result.unwrap_or(Value::Null))
                .map_err(protocol);
        }
    }

    pub async fn register_project(&mut self, path: &str) -> Result<Project, ClientError> {
        let params = rpc::RegisterProject { path: path.into() };
        self.call(method::PROJECTS_REGISTER, &params).await
    }

    pub async fn list_projects(&mut self) -> Result<Vec<Project>, ClientError> {
        self.call(method::PROJECTS_LIST, &()).await
    }

    pub async fn start_run(&mut self, params: &rpc::StartRun) -> Result<Run, ClientError> {
        self.call(method::RUNS_START, params).await
    }

    pub async fn list_runs(&mut self) -> Result<Vec<Run>, ClientError> {
        self.call(method::RUNS_LIST, &rpc::ListRuns::default())
            .await
    }

    pub async fn get_run(&mut self, run_id: &str) -> Result<rpc::RunDetail, ClientError> {
        let params = rpc::RunRef {
            run_id: run_id.into(),
        };
        self.call(method::RUNS_GET, &params).await
    }

    pub async fn cancel_run(&mut self, run_id: &str) -> Result<Run, ClientError> {
        let params = rpc::RunRef {
            run_id: run_id.into(),
        };
        self.call(method::RUNS_CANCEL, &params).await
    }

    /// Sessions, optionally only those of one run.
    pub async fn list_sessions(
        &mut self,
        run_id: Option<&str>,
    ) -> Result<Vec<Session>, ClientError> {
        let params = rpc::ListSessions {
            run_id: run_id.map(str::to_string),
        };
        self.call(method::SESSIONS_LIST, &params).await
    }

    pub async fn list_requests(
        &mut self,
        pending: bool,
    ) -> Result<Vec<PermissionRequest>, ClientError> {
        self.call(method::REQUESTS_LIST, &rpc::ListRequests { pending })
            .await
    }

    pub async fn approve(
        &mut self,
        request_id: &str,
        answer: Option<String>,
    ) -> Result<PermissionRequest, ClientError> {
        let params = rpc::Resolve {
            request_id: request_id.into(),
            answer,
        };
        self.call(method::REQUESTS_APPROVE, &params).await
    }

    pub async fn deny(
        &mut self,
        request_id: &str,
        answer: Option<String>,
    ) -> Result<PermissionRequest, ClientError> {
        let params = rpc::Resolve {
            request_id: request_id.into(),
            answer,
        };
        self.call(method::REQUESTS_DENY, &params).await
    }

    /// Turns the connection into an event stream.
    pub async fn subscribe(mut self, params: &rpc::Subscribe) -> Result<Subscription, ClientError> {
        let _: rpc::Subscribed = self.call(method::EVENTS_SUBSCRIBE, params).await?;
        Ok(Subscription { client: self })
    }
}

pub struct Subscription {
    client: Client,
}

impl Subscription {
    /// The next event, or `None` when the daemon closes the connection.
    pub async fn next(&mut self) -> Result<Option<Event>, ClientError> {
        while let Some(line) = self.client.lines.next_line().await? {
            let notification: rpc::Notification = serde_json::from_str(&line).map_err(protocol)?;
            if notification.method == method::EVENT {
                return serde_json::from_value(notification.params)
                    .map(Some)
                    .map_err(protocol);
            }
        }
        Ok(None)
    }
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    /// The daemon answered with an error.
    Rpc(rpc::Error),
    /// The daemon sent something that is not valid for this protocol.
    Protocol(String),
}

fn protocol(e: serde_json::Error) -> ClientError {
    ClientError::Protocol(e.to_string())
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Rpc(e) => write!(f, "{e}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}
