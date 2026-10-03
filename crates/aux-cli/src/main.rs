//! `aux`, the terminal twin of the AgentUX cockpit.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agentux_config::{Config, ConfigError, FILE_NAME};
use agentux_worktree::Worktrees;
use clap::{Args, Parser, Subcommand};

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(name = "aux", version, about = "AgentUX command-line interface")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check an agentux.yaml without starting a run
    Validate {
        /// File to check, or a project directory containing agentux.yaml
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Create, list and remove run worktrees by hand (development aid)
    #[command(subcommand)]
    Worktree(WorktreeCommand),
    /// Start a run from an issue or prompt (not implemented yet)
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum WorktreeCommand {
    /// Create branch aux/<RUN_ID> and check it out in a new worktree
    Create {
        run_id: String,
        /// Commit or branch to start from
        #[arg(long, default_value = "HEAD")]
        from: String,
        /// Directory for run worktrees [default: <repo>/../<repo-name>.worktrees]
        #[arg(long)]
        base_dir: Option<PathBuf>,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// List run worktrees
    List {
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Remove a run's worktree; the branch is kept unless --delete-branch is given
    Remove {
        run_id: String,
        /// Remove despite uncommitted changes, and delete the branch even if unmerged
        #[arg(long)]
        force: bool,
        /// Also delete the aux/<RUN_ID> branch
        #[arg(long)]
        delete_branch: bool,
        #[command(flatten)]
        repo: RepoArg,
    },
}

#[derive(Args)]
struct RepoArg {
    /// Repository to operate on
    #[arg(long, default_value = ".")]
    repo: PathBuf,
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Validate { path } => validate(&path),
        Command::Worktree(command) => worktree(command),
        Command::Run { .. } => Err("`aux run` is not implemented yet".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn validate(path: &Path) -> Result {
    let file = if path.is_dir() {
        let file = path.join(FILE_NAME);
        if !file.exists() {
            println!(
                "{}: no {FILE_NAME}; the built-in default pipeline applies ({})",
                path.display(),
                summary(&Config::builtin_default())
            );
            return Ok(());
        }
        file
    } else {
        path.to_path_buf()
    };

    let config = Config::from_file(&file).map_err(|e| match e {
        ConfigError::Read { .. } => e.to_string(),
        _ => format!("{} is invalid: {e}", file.display()),
    })?;
    println!("{}: valid ({})", file.display(), summary(&config));
    Ok(())
}

fn summary(config: &Config) -> String {
    let steps: Vec<&str> = config.pipeline.iter().map(|s| s.kind().as_str()).collect();
    steps.join(" -> ")
}

fn worktree(command: WorktreeCommand) -> Result {
    match command {
        WorktreeCommand::Create {
            run_id,
            from,
            base_dir,
            repo,
        } => {
            let mut worktrees = Worktrees::open(&repo.repo)?;
            if let Some(dir) = base_dir {
                worktrees = worktrees.with_base_dir(dir)?;
            }
            let worktree = worktrees.create(&run_id, &from)?;
            println!("{}\t{}", worktree.branch, worktree.path.display());
        }
        WorktreeCommand::List { repo } => {
            for worktree in Worktrees::open(&repo.repo)?.list()? {
                println!(
                    "{}\t{}\t{}",
                    worktree.run_id,
                    worktree.branch,
                    worktree.path.display()
                );
            }
        }
        WorktreeCommand::Remove {
            run_id,
            force,
            delete_branch,
            repo,
        } => {
            Worktrees::open(&repo.repo)?.remove(&run_id, force, delete_branch)?;
        }
    }
    Ok(())
}
