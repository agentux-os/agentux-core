//! The `pull_request` step on GitHub: push the run's branch and open a pull
//! request with `gh`, or find the one already open for the branch. Projects
//! without a GitHub `origin`, or machines without an authenticated `gh`, get
//! no pull request; the branch stays for the human.

use std::path::Path;
use std::process::Stdio;

use agentux_api::PullRequest;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::executor::{PullRequestOutcome, PullRequestTask};

/// Keep pull request bodies well under GitHub's 65536-character limit.
const MAX_SECTION: usize = 20_000;

pub async fn open_pull_request(task: &PullRequestTask) -> Result<PullRequestOutcome, String> {
    let dir = &task.worktree;
    let remote = match output(dir, "git", &["remote", "get-url", "origin"], None).await {
        Ok(url) => url.trim().to_string(),
        Err(_) => return Ok(skip("the project has no `origin` remote")),
    };
    if !is_github(&remote) {
        return Ok(skip(&format!("origin ({remote}) is not on GitHub")));
    }
    match output(
        dir,
        "gh",
        &["auth", "status", "--hostname", "github.com"],
        None,
    )
    .await
    {
        Ok(_) => {}
        Err(Failure::Spawn(_)) => return Ok(skip("the GitHub CLI (gh) is not installed")),
        Err(Failure::Exit(_)) => {
            return Ok(skip(
                "gh is not logged in to github.com (run `gh auth login`)",
            ));
        }
    }

    output(
        dir,
        "git",
        &["push", "--set-upstream", "origin", &task.branch],
        None,
    )
    .await
    .map_err(|e| format!("git push failed: {e}"))?;

    // Idempotent: a pull request already open for the branch is reused.
    let existing = output(
        dir,
        "gh",
        &[
            "pr",
            "list",
            "--head",
            &task.branch,
            "--state",
            "open",
            "--json",
            "number,url",
        ],
        None,
    )
    .await
    .map_err(|e| format!("gh pr list failed: {e}"))?;
    let existing: Vec<PullRequest> = serde_json::from_str::<Vec<Listed>>(&existing)
        .map_err(|e| format!("cannot read gh pr list output: {e}"))?
        .into_iter()
        .map(|p| PullRequest {
            number: p.number,
            url: p.url,
        })
        .collect();
    if let Some(pr) = existing.into_iter().next() {
        return Ok(PullRequestOutcome::Opened(pr));
    }

    let body = body(task);
    let mut args = vec![
        "pr",
        "create",
        "--head",
        &task.branch,
        "--title",
        &task.title,
        "--body-file",
        "-",
    ];
    if task.draft {
        args.push("--draft");
    }
    let created = output(dir, "gh", &args, Some(&body))
        .await
        .map_err(|e| format!("gh pr create failed: {e}"))?;
    let url = created
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("https://"))
        .ok_or_else(|| format!("gh pr create printed no URL: {created}"))?;
    let number =
        number_from_url(url).ok_or_else(|| format!("unexpected pull request URL {url}"))?;
    Ok(PullRequestOutcome::Opened(PullRequest {
        number,
        url: url.to_string(),
    }))
}

#[derive(Deserialize)]
struct Listed {
    number: u64,
    url: String,
}

fn skip(reason: &str) -> PullRequestOutcome {
    PullRequestOutcome::Skipped(reason.to_string())
}

/// `https://github.com/o/r(.git)`, `git@github.com:o/r.git`,
/// `ssh://git@github.com/o/r.git`.
pub fn is_github(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split(':').next().unwrap_or_default();
    host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com")
}

pub fn number_from_url(url: &str) -> Option<u64> {
    let (_, tail) = url.trim_end_matches('/').rsplit_once("/pull/")?;
    tail.split(['/', '#', '?']).next()?.parse().ok()
}

/// The pull request description: the request, the plan and the review.
pub fn body(task: &PullRequestTask) -> String {
    let mut out = String::new();
    if let Some(prompt) = &task.prompt {
        out.push_str("## Request\n\n");
        out.push_str(&cut(prompt.trim()));
        out.push_str("\n\n");
    }
    if let Some(issue) = task.issue {
        out.push_str(&format!("Closes #{issue}\n\n"));
    }
    if let Some(plan) = &task.plan {
        out.push_str("## Plan\n\n");
        out.push_str(&cut(plan.trim()));
        out.push_str("\n\n");
    }
    if let Some(review) = &task.review {
        out.push_str("## Review\n\n");
        out.push_str(&cut(review.trim()));
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "---\nOpened by AgentUX run {} from branch `{}`.\n",
        task.run_id, task.branch
    ));
    out
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(MAX_SECTION) {
        Some((i, _)) => format!("{}\n\n[…]", &text[..i]),
        None => text.to_string(),
    }
}

enum Failure {
    Spawn(String),
    Exit(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(m) | Self::Exit(m) => f.write_str(m),
        }
    }
}

/// Runs a command in `dir` without a terminal and returns its stdout.
async fn output(
    dir: &Path,
    program: &str,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<String, Failure> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Failure::Spawn(format!("cannot run {program}: {e}")))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())
            .await
            .map_err(|e| Failure::Exit(format!("cannot write to {program}: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| Failure::Exit(format!("{program}: {e}")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(Failure::Exit(format!(
            "{program} {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn github_remotes() {
        for url in [
            "https://github.com/agentux-os/agentux-core.git",
            "https://github.com/agentux-os/agentux-core",
            "git@github.com:agentux-os/agentux-core.git",
            "ssh://git@github.com/agentux-os/agentux-core.git",
            "https://user:token@github.com/o/r.git",
        ] {
            assert!(is_github(url), "{url}");
        }
        for url in [
            "https://gitlab.com/o/r.git",
            "git@example.com:github.com/r.git",
            "/srv/git/repo.git",
            "https://github.com.evil.example/o/r",
        ] {
            assert!(!is_github(url), "{url}");
        }
    }

    #[test]
    fn pull_request_numbers() {
        assert_eq!(number_from_url("https://github.com/o/r/pull/42"), Some(42));
        assert_eq!(
            number_from_url("https://github.com/o/r/pull/7/files"),
            Some(7)
        );
        assert_eq!(number_from_url("https://github.com/o/r/issues/7"), None);
    }

    #[test]
    fn body_has_request_plan_and_review() {
        let body = body(&PullRequestTask {
            run_id: "3f9a0c12".into(),
            title: "Add health".into(),
            branch: "aux/3f9a0c12".into(),
            worktree: PathBuf::from("/tmp"),
            draft: false,
            prompt: Some("Add a health endpoint".into()),
            issue: Some(5),
            plan: Some("1. route".into()),
            review: Some("Looks right.".into()),
        });
        assert!(body.starts_with(
            "## Request\n\nAdd a health endpoint\n\nCloses #5\n\n## Plan\n\n1. route"
        ));
        assert!(body.contains("## Review\n\nLooks right."));
        assert!(body.contains("AgentUX run 3f9a0c12"));
    }
}
