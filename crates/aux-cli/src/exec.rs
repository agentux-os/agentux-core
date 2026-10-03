//! `aux exec`: one prompt against one harness over ACP, for trying harnesses
//! by hand. Events stream to the terminal; permission requests ask y/n.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentux_harness::{
    AcpHarness, Decision, Event, Harness, HarnessSession, HarnessSpec, PermissionHandler,
    PermissionRequest, PlanStatus, ToolStatus, permission_handler,
};
use tokio::sync::Mutex;

use crate::Result;

pub fn exec(harness: &str, cwd: &Path, prompt: &str) -> Result {
    let Some(spec) = HarnessSpec::find(harness) else {
        let known: Vec<String> = HarnessSpec::builtin().into_iter().map(|s| s.id).collect();
        return Err(format!("unknown harness `{harness}` (known: {})", known.join(", ")).into());
    };
    if spec.experimental {
        eprintln!("warning: the `{}` harness is experimental", spec.id);
    }
    // ACP requires an absolute working directory.
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("cannot use {} as working directory: {e}", cwd.display()))?;
    tokio::runtime::Runtime::new()?.block_on(run(spec, cwd, prompt))
}

async fn run(spec: HarnessSpec, cwd: PathBuf, prompt: &str) -> Result {
    eprintln!("[start] {} in {}", spec.command_line(), cwd.display());
    let (session, mut events) = AcpHarness::new(spec).start(&cwd, ask_on_terminal()).await?;
    let mut out = Printer::default();

    let stop = {
        let turn = session.prompt(prompt);
        tokio::pin!(turn);
        let mut cancelling = false;
        loop {
            tokio::select! {
                Some(event) = events.recv() => out.print(event),
                stop = &mut turn => break stop,
                _ = tokio::signal::ctrl_c() => {
                    if cancelling {
                        // Dropping the session kills the harness.
                        return Err("interrupted".into());
                    }
                    cancelling = true;
                    out.line("[cancel] asking the agent to stop; Ctrl-C again to quit");
                    session.cancel()?;
                }
            }
        }
    };
    while let Ok(event) = events.try_recv() {
        out.print(event);
    }
    out.line(&format!("[stop] {:?}", stop?));
    session.shutdown().await?;
    Ok(())
}

/// Asks y/n on the terminal, one question at a time.
fn ask_on_terminal() -> PermissionHandler {
    let lock = Arc::new(Mutex::new(()));
    permission_handler(move |request: PermissionRequest| {
        let lock = Arc::clone(&lock);
        async move {
            let _turn = lock.lock().await;
            let what = request.title.as_deref().unwrap_or(&request.tool_call_id);
            let question = match request.kind {
                Some(kind) => format!("\n[permission] {what} ({kind:?}) - allow? [y/N] "),
                None => format!("\n[permission] {what} - allow? [y/N] "),
            };
            let answer = tokio::task::spawn_blocking(move || {
                eprint!("{question}");
                let mut line = String::new();
                io::stdin().lock().read_line(&mut line).map(|_| line)
            })
            .await;
            match answer {
                Ok(Ok(line)) if matches!(line.trim(), "y" | "Y" | "yes") => Decision::Allow,
                _ => Decision::Deny,
            }
        }
    })
}

/// Prints events, keeping streamed message text on its own lines.
#[derive(Default)]
struct Printer {
    /// The cursor is in the middle of streamed text.
    mid_line: bool,
}

impl Printer {
    fn print(&mut self, event: Event) {
        match event {
            Event::AgentMessage(text) => {
                print!("{text}");
                let _ = io::stdout().flush();
                self.mid_line = !text.ends_with('\n');
            }
            Event::AgentThought(text) => self.line(&format!("[thought] {}", text.trim_end())),
            Event::ToolCall(call) => self.line(&format!(
                "[tool {}] {:?}: {} ({})",
                call.id,
                call.kind,
                call.title,
                status(call.status)
            )),
            Event::ToolCallUpdate(update) => {
                let mut line = format!("[tool {}]", update.id);
                if let Some(title) = &update.title {
                    line.push_str(&format!(" {title}"));
                }
                if let Some(s) = update.status {
                    line.push_str(&format!(" ({})", status(s)));
                }
                self.line(&line);
                if let Some(output) = update.output {
                    for text in output.lines() {
                        self.line(&format!("    {text}"));
                    }
                }
            }
            Event::Plan(entries) => {
                self.line("[plan]");
                for entry in entries {
                    let mark = match entry.status {
                        PlanStatus::Completed => "[x]",
                        PlanStatus::InProgress => "[>]",
                        PlanStatus::Pending => "[ ]",
                    };
                    self.line(&format!("  {mark} {}", entry.content));
                }
            }
            Event::Diff(diff) => {
                let change = if diff.old_text.is_some() {
                    "edit"
                } else {
                    "new file"
                };
                self.line(&format!(
                    "[diff] {} ({change}, {} lines)",
                    diff.path.display(),
                    diff.new_text.lines().count()
                ));
            }
            Event::Usage(usage) => {
                let mut line = format!(
                    "[usage] {}/{} tokens",
                    usage.used_tokens, usage.context_tokens
                );
                if let Some(cost) = usage.cost {
                    line.push_str(&format!(", {:.4} {}", cost.amount, cost.currency));
                }
                self.line(&line);
            }
        }
    }

    fn line(&mut self, text: &str) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
        println!("{text}");
    }
}

fn status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "pending",
        ToolStatus::InProgress => "in progress",
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
    }
}
