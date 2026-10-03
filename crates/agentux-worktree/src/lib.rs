//! Git worktrees for AgentUX runs. Each run gets its own branch,
//! `aux/<run-id>`, checked out in its own worktree under a base directory.
//! Everything shells out to `git`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fmt, fs, io};

/// Prefix of every run branch.
pub const BRANCH_PREFIX: &str = "aux/";

/// A run's worktree, as reported by `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub run_id: String,
    /// Short branch name, e.g. `aux/42`.
    pub branch: String,
    pub path: PathBuf,
    /// Commit checked out in the worktree.
    pub head: String,
}

/// The run worktrees of one repository.
#[derive(Debug, Clone)]
pub struct Worktrees {
    repo: PathBuf,
    base_dir: PathBuf,
}

impl Worktrees {
    /// Opens the repository that contains `path`. New worktrees go to
    /// `<repo-parent>/<repo-name>.worktrees/<run-id>` unless
    /// [`Worktrees::with_base_dir`] says otherwise.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let repo = PathBuf::from(git(path, ["rev-parse", "--show-toplevel"])?.trim());
        let name = repo
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy());
        let base_dir = repo
            .parent()
            .unwrap_or(&repo)
            .join(format!("{name}.worktrees"));
        Ok(Self { repo, base_dir })
    }

    /// Puts new worktrees under `base_dir` (made absolute against the current
    /// directory).
    pub fn with_base_dir(mut self, base_dir: impl AsRef<Path>) -> io::Result<Self> {
        self.base_dir = std::path::absolute(base_dir)?;
        Ok(self)
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Creates branch `aux/<run_id>` at `start_point` (a commit, branch or
    /// `HEAD`) and checks it out in `<base_dir>/<run_id>`.
    pub fn create(&self, run_id: &str, start_point: &str) -> Result<Worktree, Error> {
        validate_run_id(run_id)?;
        let branch = format!("{BRANCH_PREFIX}{run_id}");
        let path = self.base_dir.join(run_id);
        fs::create_dir_all(&self.base_dir).map_err(Error::Io)?;
        git(
            &self.repo,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--quiet"),
                OsStr::new("-b"),
                OsStr::new(&branch),
                path.as_os_str(),
                OsStr::new(start_point),
            ],
        )?;
        self.find(run_id)?
            .ok_or_else(|| Error::NotFound(run_id.to_string()))
    }

    /// Lists the worktrees whose branch is a run branch (`aux/*`).
    pub fn list(&self) -> Result<Vec<Worktree>, Error> {
        let output = git(&self.repo, ["worktree", "list", "--porcelain"])?;
        Ok(parse_porcelain(&output))
    }

    pub fn find(&self, run_id: &str) -> Result<Option<Worktree>, Error> {
        Ok(self.list()?.into_iter().find(|w| w.run_id == run_id))
    }

    /// Removes the run's worktree. The branch is kept, since it may hold work
    /// not pushed yet, unless `delete_branch` is set. Without `force`, git
    /// refuses to remove a worktree with uncommitted changes or to delete an
    /// unmerged branch.
    pub fn remove(&self, run_id: &str, force: bool, delete_branch: bool) -> Result<(), Error> {
        validate_run_id(run_id)?;
        let worktree = self
            .find(run_id)?
            .ok_or_else(|| Error::NotFound(run_id.to_string()))?;

        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(worktree.path.as_os_str());
        git(&self.repo, args)?;

        if delete_branch {
            let flag = if force { "-D" } else { "-d" };
            git(&self.repo, ["branch", "--quiet", flag, &worktree.branch])?;
        }
        Ok(())
    }
}

/// Run ids become branch and directory names, so they are restricted to
/// ASCII letters, digits, `-`, `_` and `.`, starting with a letter or digit.
fn validate_run_id(run_id: &str) -> Result<(), Error> {
    let valid = run_id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !run_id.contains("..")
        && !run_id.ends_with('.')
        && !run_id.ends_with(".lock");
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidRunId(run_id.to_string()))
    }
}

/// Parses `git worktree list --porcelain`: blocks separated by blank lines,
/// with `worktree <path>`, `HEAD <sha>` and `branch refs/heads/<name>` lines.
fn parse_porcelain(output: &str) -> Vec<Worktree> {
    output
        .split("\n\n")
        .filter_map(|block| {
            let (mut path, mut head, mut branch) = (None, None, None);
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(value));
                } else if let Some(value) = line.strip_prefix("HEAD ") {
                    head = Some(value.to_string());
                } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
                    branch = Some(value.to_string());
                }
            }
            let branch = branch?;
            let run_id = branch.strip_prefix(BRANCH_PREFIX)?.to_string();
            Some(Worktree {
                run_id,
                branch,
                path: path?,
                head: head.unwrap_or_default(),
            })
        })
        .collect()
}

/// Runs `git -C <dir> <args>` and returns its stdout.
fn git<I, S>(dir: &Path, args: I) -> Result<String, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(&args)
        .output()
        .map_err(Error::Io)?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let command = args
        .iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    Err(Error::Git {
        command,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

#[derive(Debug)]
pub enum Error {
    InvalidRunId(String),
    /// No run worktree exists for this run id.
    NotFound(String),
    /// A git command exited with an error.
    Git {
        command: String,
        stderr: String,
    },
    /// git could not be started, or a directory could not be created.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRunId(id) => write!(
                f,
                "invalid run id `{id}`: use ASCII letters, digits, `-`, `_` and `.`, \
                 starting with a letter or digit"
            ),
            Self::NotFound(id) => write!(f, "no worktree for run `{id}`"),
            Self::Git { command, stderr } => write!(f, "`git {command}` failed: {stderr}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_keeps_only_run_branches() {
        let output = "worktree /src/app\nHEAD 1111\nbranch refs/heads/main\n\n\
                      worktree /src/app.worktrees/42\nHEAD 2222\nbranch refs/heads/aux/42\n\n\
                      worktree /src/app.worktrees/old\nHEAD 3333\ndetached\n\n\
                      worktree /src/other\nHEAD 4444\nbranch refs/heads/auxiliary\n";
        assert_eq!(
            parse_porcelain(output),
            [Worktree {
                run_id: "42".into(),
                branch: "aux/42".into(),
                path: PathBuf::from("/src/app.worktrees/42"),
                head: "2222".into(),
            }]
        );
    }

    #[test]
    fn run_ids_are_safe_branch_and_directory_names() {
        for ok in ["42", "run-1", "fix_login.2", "A1"] {
            assert!(validate_run_id(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "-x", ".x", "_x", "a/b", "../x", "a..b", "x.", "x.lock", "a b", "a~1", "é",
        ] {
            assert!(validate_run_id(bad).is_err(), "{bad:?}");
        }
    }
}
