//! The API: newline-delimited JSON-RPC 2.0 over a Unix domain socket.
//! See `docs/api.md`.

use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use agentux_api::rpc::{self, code, method};
use agentux_api::{Event, rpc::Request, rpc::Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::engine::{Engine, Error};

/// Binds `socket` and serves the API until `shutdown` completes.
///
/// The socket's directory is created with mode 0700 and the socket itself
/// gets 0600: the API is for the local user only. A stale socket file left by
/// a crashed daemon is replaced; a live one is an error.
pub async fn serve(
    engine: Engine,
    socket: &Path,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let listener = bind(socket).await?;
    tokio::pin!(shutdown);
    let mut connections = Vec::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                connections.retain(|c: &JoinHandle<()>| !c.is_finished());
                connections.push(tokio::spawn(connection(engine.clone(), stream)));
            }
            () = &mut shutdown => break,
        }
    }
    for connection in connections {
        connection.abort();
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}

async fn bind(socket: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if socket.exists() {
        if UnixStream::connect(socket).await.is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another agentuxd is listening on {}", socket.display()),
            ));
        }
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves one client. Responses and event notifications share one writer, so
/// lines never interleave.
async fn connection(engine: Engine, stream: UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let write = tokio::spawn(async move {
        while let Some(mut line) = rx.recv().await {
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    let mut subscription: Option<JoinHandle<()>> = None;
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Value>(&line) {
            Err(e) => Err(Response::err(
                Value::Null,
                rpc::Error::new(code::PARSE_ERROR, e.to_string()),
            )),
            Ok(value) => {
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                serde_json::from_value::<Request>(value).map_err(|e| {
                    Response::err(id, rpc::Error::new(code::INVALID_REQUEST, e.to_string()))
                })
            }
        };
        let request = match request {
            Ok(request) if request.jsonrpc == rpc::VERSION => request,
            Ok(request) => {
                let error = rpc::Error::new(code::INVALID_REQUEST, "jsonrpc must be \"2.0\"");
                send(
                    &tx,
                    &Response::err(request.id.unwrap_or(Value::Null), error),
                );
                continue;
            }
            Err(response) => {
                send(&tx, &response);
                continue;
            }
        };

        if request.method == method::EVENTS_SUBSCRIBE {
            let result = parse_or_default::<rpc::Subscribe>(request.params)
                .and_then(|params| subscribe(&engine, params, tx.clone()));
            match result {
                Ok((subscribed, task)) => {
                    // The task only sends once it holds the response's place
                    // in the queue (it is spawned after this send).
                    reply(&tx, request.id, Ok(subscribed));
                    if let Some(old) = subscription.replace(tokio::spawn(task)) {
                        old.abort();
                    }
                }
                Err(e) => reply::<()>(&tx, request.id, Err(e)),
            }
            continue;
        }

        let result = handle(&engine, &request.method, request.params).await;
        reply(&tx, request.id, result);
    }

    if let Some(task) = subscription {
        task.abort();
    }
    drop(tx);
    let _ = write.await;
}

async fn handle(engine: &Engine, method: &str, params: Value) -> Result<Value, rpc::Error> {
    match method {
        method::PROJECTS_REGISTER => {
            let params: rpc::RegisterProject = parse(params)?;
            to_value(engine.register_project(&params.path).await)
        }
        method::PROJECTS_LIST => to_value(engine.projects()),
        method::RUNS_START => to_value(engine.start_run(parse(params)?)),
        method::RUNS_LIST => {
            let params: rpc::ListRuns = parse_or_default(params)?;
            to_value(engine.runs(params.project_id.as_deref()))
        }
        method::RUNS_GET => {
            let params: rpc::RunRef = parse(params)?;
            to_value(
                engine
                    .run(&params.run_id)
                    .and_then(|(run, attempts, requests)| {
                        Ok(rpc::RunDetail {
                            sessions: engine.sessions(Some(&run.id))?,
                            run,
                            attempts,
                            requests,
                        })
                    }),
            )
        }
        method::SESSIONS_LIST => {
            let params: rpc::ListSessions = parse_or_default(params)?;
            to_value(engine.sessions(params.run_id.as_deref()))
        }
        method::RUNS_CANCEL => {
            let params: rpc::RunRef = parse(params)?;
            to_value(engine.cancel(&params.run_id))
        }
        method::REQUESTS_LIST => {
            let params: rpc::ListRequests = parse_or_default(params)?;
            to_value(engine.requests(params.pending))
        }
        method::REQUESTS_APPROVE => {
            let params: rpc::Resolve = parse(params)?;
            to_value(engine.approve(&params.request_id, params.answer.as_deref()))
        }
        method::REQUESTS_DENY => {
            let params: rpc::Resolve = parse(params)?;
            to_value(engine.deny(&params.request_id, params.answer.as_deref()))
        }
        method::BUS_LIST => {
            let params: rpc::ListBus = parse(params)?;
            to_value(engine.bus_list(&params.run_id))
        }
        // The bridge behind `aux bus-stdio`. A `bus.call` can wait minutes
        // (ask_human), which holds up later requests on this connection, so
        // the bridge sends each call on a connection of its own.
        method::BUS_HELLO => {
            let params: agentux_bus::BridgeHello = parse(params)?;
            to_value(engine.bus_hello(&params.session_token))
        }
        method::BUS_CALL => {
            let params: agentux_bus::BridgeCall = parse(params)?;
            to_value(engine.bus_call(&params.session_token, params.call).await)
        }
        _ => Err(rpc::Error::new(
            code::METHOD_NOT_FOUND,
            format!("unknown method {method:?}"),
        )),
    }
}

/// Prepares a subscription: listens for live events first, then reads what
/// is already persisted, so nothing committed in between is lost. The
/// returned task forwards events in `seq` order without duplicates.
fn subscribe(
    engine: &Engine,
    params: rpc::Subscribe,
    tx: mpsc::UnboundedSender<String>,
) -> Result<(rpc::Subscribed, impl Future<Output = ()> + use<>), rpc::Error> {
    let store = engine.store().clone();
    let mut live = store.subscribe();
    let run_id = params.run_id;
    let (head, backlog) = store
        .read(|t| {
            let head = t.last_event_seq()?;
            let backlog = match params.since {
                Some(since) => t.events_since(since, run_id.as_deref())?,
                None => Vec::new(),
            };
            Ok::<_, Error>((head, backlog))
        })
        .map_err(rpc_error)?;

    let task = async move {
        let mut last = params.since.unwrap_or(head);
        let forward = |event: &Event, last: &mut i64| -> bool {
            if event.seq <= *last {
                return true;
            }
            *last = event.seq;
            if run_id.is_some() && event.run_id != run_id {
                return true;
            }
            notify(&tx, event)
        };
        for event in &backlog {
            if !forward(event, &mut last) {
                return;
            }
        }
        loop {
            match live.recv().await {
                Ok(event) => {
                    if !forward(&event, &mut last) {
                        return;
                    }
                }
                // Too slow to keep up: catch up from the database.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let Ok(missed) = store.read(|t| t.events_since(last, run_id.as_deref())) else {
                        return;
                    };
                    for event in &missed {
                        if !forward(event, &mut last) {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    };
    Ok((rpc::Subscribed { seq: head }, task))
}

fn notify(tx: &mpsc::UnboundedSender<String>, event: &Event) -> bool {
    let notification = rpc::Notification {
        jsonrpc: rpc::VERSION.into(),
        method: method::EVENT.into(),
        params: serde_json::to_value(event).unwrap_or(Value::Null),
    };
    match serde_json::to_string(&notification) {
        Ok(line) => tx.send(line).is_ok(),
        Err(_) => true,
    }
}

fn reply<T: Serialize>(
    tx: &mpsc::UnboundedSender<String>,
    id: Option<Value>,
    result: Result<T, rpc::Error>,
) {
    // Notifications (no id) get no response.
    let Some(id) = id else { return };
    let response = match result.and_then(|v| {
        serde_json::to_value(v).map_err(|e| rpc::Error::new(code::INTERNAL_ERROR, e.to_string()))
    }) {
        Ok(value) => Response::ok(id, value),
        Err(error) => Response::err(id, error),
    };
    send(tx, &response);
}

fn send(tx: &mpsc::UnboundedSender<String>, response: &Response) {
    if let Ok(line) = serde_json::to_string(response) {
        let _ = tx.send(line);
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, rpc::Error> {
    serde_json::from_value(params).map_err(|e| rpc::Error::new(code::INVALID_PARAMS, e.to_string()))
}

fn parse_or_default<T: DeserializeOwned + Default>(params: Value) -> Result<T, rpc::Error> {
    if params.is_null() {
        Ok(T::default())
    } else {
        parse(params)
    }
}

fn to_value<T: Serialize>(result: Result<T, Error>) -> Result<Value, rpc::Error> {
    let value = result.map_err(rpc_error)?;
    serde_json::to_value(value).map_err(|e| rpc::Error::new(code::INTERNAL_ERROR, e.to_string()))
}

fn rpc_error(e: Error) -> rpc::Error {
    let code = match &e {
        Error::NotFound(_) => code::NOT_FOUND,
        Error::Conflict(_) => code::CONFLICT,
        Error::InvalidProject(_) => code::INVALID_PROJECT,
        Error::InvalidParams(_) => code::INVALID_PARAMS,
        Error::Unauthorized(_) => code::UNAUTHORIZED,
        Error::Internal(_) => code::INTERNAL_ERROR,
    };
    rpc::Error::new(code, e.to_string())
}
