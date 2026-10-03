//! `aux`, the terminal twin of the AgentUX cockpit.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agentux_config::{Config, ConfigError, FILE_NAME};
use agentux_worktree::Worktrees;
use clap::{Args, Parser, Subcommand};

mod exec;
mod remote;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

#[derive(Parser)]
#[command(name = "aux", version, about = "AgentUX command-line interface")]
struct Cli {
    /// agentuxd socket [default: $AGENTUX_SOCKET, else
    /// $XDG_RUNTIME_DIR/agentux/agentuxd.sock]
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
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
    /// Send one prompt to a harness over ACP and stream what it does
    /// (development aid; asks y/n for each permission request)
    Exec {
        /// Harness to run: claude-code, codex, opencode or antigravity
        #[arg(long)]
        harness: String,
        /// Working directory of the harness
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        prompt: String,
    },
    /// Run the daemon in the foreground
    Daemon {
        /// SQLite database [default: $XDG_STATE_HOME/agentux/agentuxd.db, else
        /// ~/.local/state/agentux/agentuxd.db]
        #[arg(long)]
        database: Option<PathBuf>,
        /// Answer agent steps with a scripted fake instead of a harness
        #[arg(long)]
        fake_agents: bool,
    },
    /// Start a run in a project
    Run {
        /// Project directory (inside a git repository)
        #[arg(default_value = ".")]
        project_dir: PathBuf,
        /// What to do
        #[arg(long, short)]
        prompt: Option<String>,
        /// Issue number on the project's forge
        #[arg(long, short)]
        issue: Option<u64>,
        /// Run title [default: first line of the prompt, or "Issue #N"]
        #[arg(long)]
        title: Option<String>,
        /// Follow the run after starting it, like `aux watch`
        #[arg(long, short)]
        watch: bool,
    },
    /// List active runs and pending approvals
    Ps {
        /// Include finished runs
        #[arg(long, short)]
        all: bool,
    },
    /// Approve a pending request
    Approve {
        request_id: String,
        /// Note recorded with the decision
        #[arg(long, short)]
        message: Option<String>,
    },
    /// Deny a pending request; its run fails
    Deny {
        request_id: String,
        /// Reason recorded with the decision
        #[arg(long, short)]
        message: Option<String>,
    },
    /// Follow a run's events until it finishes
    Watch { run_id: String },
    /// Cancel a run (its worktree and branch are kept)
    Cancel { run_id: String },
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
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Validate { path } => validate(&path),
        Command::Worktree(command) => worktree(command),
        Command::Exec {
            harness,
            cwd,
            prompt,
        } => exec::exec(&harness, &cwd, &prompt),
        Command::Daemon {
            database,
            fake_agents,
        } => daemon(agentuxd::Options {
            socket: cli.socket,
            database,
            fake_agents,
        }),
        command => remote(cli.socket, command),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn daemon(options: agentuxd::Options) -> Result {
    tokio::runtime::Runtime::new()?.block_on(agentuxd::run(options))
}

fn remote(socket: Option<PathBuf>, command: Command) -> Result {
    let remote = remote::Remote::new(socket)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        match command {
            Command::Run {
                project_dir,
                prompt,
                issue,
                title,
                watch,
            } => remote.run(&project_dir, prompt, issue, title, watch).await,
            Command::Ps { all } => remote.ps(all).await,
            Command::Approve {
                request_id,
                message,
            } => remote.resolve(&request_id, true, message).await,
            Command::Deny {
                request_id,
                message,
            } => remote.resolve(&request_id, false, message).await,
            Command::Watch { run_id } => remote.watch(&run_id).await,
            Command::Cancel { run_id } => remote.cancel(&run_id).await,
            Command::Validate { .. }
            | Command::Worktree(_)
            | Command::Exec { .. }
            | Command::Daemon { .. } => {
                unreachable!("handled locally")
            }
        }
    })
}

fn validate(path: &Path) -> Result {
    let file = if path.is_dir() {
        let file = path.join(FILE_NAME);
        if !file.exists() {
            let config = Config::default_for(path);
            println!(
                "{}: no {FILE_NAME}; the built-in default pipeline applies ({})",
                path.display(),
                summary(&config)
            );
            if config.checks.is_empty() {
                println!("no lint or test commands detected; the gate step is left out");
            } else {
                println!("detected checks:");
                print_checks(&config);
            }
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
    if !config.checks.is_empty() {
        println!("checks:");
        print_checks(&config);
    }
    Ok(())
}

fn summary(config: &Config) -> String {
    let steps: Vec<&str> = config.pipeline.iter().map(|s| s.kind().as_str()).collect();
    steps.join(" -> ")
}

fn print_checks(config: &Config) {
    for check in &config.checks {
        println!("  {}: {}", check.name, check.run);
    }
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
