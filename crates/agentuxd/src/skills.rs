//! The tool catalog's skills (ADR 0026 in agentux-os/agentux): Agent Skills
//! directories copied into a content-addressed store and checked there.
//!
//! A skill comes from a local directory (`path`) or a Git commit (`git`,
//! `rev`, `subdir`). It is copied into a temporary directory of the store
//! with the limits checked while copying (size, files, `SKILL.md`), only
//! regular files and directories are taken, and the copy's digest names it:
//! `<store>/sha256-<hex>/`, read-only. So the store's contents always match
//! their names, whatever the source does meanwhile, and a catalog entry with
//! a `digest` gets exactly that content or nothing.
//!
//! The digest is SHA-256 over a canonical listing of the tree: for each file,
//! in byte order of its path relative to the skill, the path, whether it is
//! executable and its contents' SHA-256. File times, owners and other modes
//! are not part of it.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentux_config::{MAX_SKILL_BYTES, MAX_SKILL_FILES, MAX_SKILL_MD_BYTES, Skill, ToolSource};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The store, under the user's data directory.
pub const STORE_DIR: &str = "agentux/skills";

/// A skill in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// `<store>/sha256-<hex>`.
    pub dir: PathBuf,
    /// `sha256:<hex>`.
    pub digest: String,
    pub bytes: u64,
    pub files: usize,
    /// The `description` from its `SKILL.md`.
    pub description: String,
}

/// The content-addressed skills store.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$XDG_DATA_HOME/agentux/skills`, else `~/.local/share/agentux/skills`.
    pub fn user() -> Option<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|home| PathBuf::from(home).join(".local/share"))
            })?;
        Some(Self::new(base.join(STORE_DIR)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Copies `skill` into the store (from its directory, or fetched from
    /// its Git commit) and checks it. `project` is the repository a project
    /// catalog's relative `path` is in.
    pub fn resolve(&self, skill: &Skill, project: Option<&Path>) -> Result<Stored, String> {
        fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create {}: {e}", self.root.display()))?;
        let work = tempfile::Builder::new()
            .prefix(".tmp-")
            .tempdir_in(&self.root)
            .map_err(|e| format!("cannot create a directory in {}: {e}", self.root.display()))?;
        let copy = work.path().join("skill");
        match (&skill.path, &skill.git) {
            (Some(path), None) => {
                let source = source_dir(path, skill.source, project)?;
                copy_checked(&source, &copy, false)?;
            }
            (None, Some(url)) => {
                let rev = skill.rev.as_deref().ok_or("a `git` skill needs `rev`")?;
                let checkout = work.path().join("checkout");
                fetch(url, rev, skill.subdir.as_deref(), &checkout)?;
                let source = match &skill.subdir {
                    Some(subdir) => checkout.join(subdir),
                    None => checkout,
                };
                copy_checked(&source, &copy, true)?;
            }
            _ => return Err("a skill needs exactly one source".into()),
        }
        let tree = check(&copy, Some(&skill.name))?;
        if let Some(want) = &skill.digest
            && *want != tree.digest
        {
            return Err(format!(
                "its content does not match the catalog's digest (got {})",
                tree.digest
            ));
        }
        let dir = self.dir_of(&tree.digest);
        if !dir.exists() {
            // Everything read-only but the top directory: renaming a
            // directory needs write access to it (its `..` entry changes).
            read_only(&copy)
                .and_then(|()| fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)))
                .map_err(|e| format!("cannot seal the copy: {e}"))?;
            match fs::rename(&copy, &dir) {
                Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o555))
                    .map_err(|e| format!("cannot seal {}: {e}", dir.display()))?,
                // An identical copy got there first.
                Err(_) if dir.exists() => {}
                Err(e) => return Err(format!("cannot store it in {}: {e}", dir.display())),
            }
        }
        // `work` and anything left in it are removed here; a sealed copy that
        // lost the race to an identical one needs its write bits back first.
        let _ = make_writable(work.path());
        Ok(Stored {
            dir,
            digest: tree.digest,
            bytes: tree.bytes,
            files: tree.files,
            description: tree.description,
        })
    }

    /// Where the skill with `digest` is stored.
    pub fn dir_of(&self, digest: &str) -> PathBuf {
        self.root.join(digest.replacen("sha256:", "sha256-", 1))
    }
}

/// A skill directory checked against the limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub digest: String,
    pub bytes: u64,
    pub files: usize,
    pub description: String,
}

/// Checks `dir` as a skill and returns its digest: only regular files and
/// directories, within the limits, with a `SKILL.md` whose frontmatter has
/// a `description` of 1-1024 characters and, when `name` is given, a `name`
/// that is absent or equal to it.
pub fn check(dir: &Path, name: Option<&str>) -> Result<Tree, String> {
    let mut files = Vec::new();
    let mut bytes = 0;
    walk(dir, dir, false, &mut files, &mut bytes)?;
    files.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));

    let skill_md = dir.join("SKILL.md");
    let meta = fs::symlink_metadata(&skill_md).map_err(|_| "it has no SKILL.md".to_string())?;
    if !meta.is_file() {
        return Err("its SKILL.md is not a regular file".into());
    }
    if meta.len() > MAX_SKILL_MD_BYTES {
        return Err(format!(
            "its SKILL.md is {} KiB, over the limit of {} KiB",
            meta.len().div_ceil(1024),
            MAX_SKILL_MD_BYTES / 1024
        ));
    }
    let text = fs::read_to_string(&skill_md).map_err(|e| format!("cannot read SKILL.md: {e}"))?;
    let front = frontmatter(&text)?;
    let description = front
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .ok_or("SKILL.md's frontmatter has no description")?;
    if description.chars().count() > 1024 {
        return Err("SKILL.md's description is over 1024 characters".into());
    }
    if let (Some(want), Some(got)) = (name, front.name.as_deref())
        && want != got
    {
        return Err(format!("SKILL.md names the skill `{got}`, not `{want}`"));
    }

    let mut listing = Sha256::new();
    for (relative, executable) in &files {
        let mut hasher = Sha256::new();
        let mut file = fs::File::open(dir.join(relative))
            .map_err(|e| format!("cannot read {}: {e}", relative.display()))?;
        io::copy(&mut file, &mut hasher)
            .map_err(|e| format!("cannot read {}: {e}", relative.display()))?;
        listing.update(relative.as_os_str().as_bytes());
        listing.update(if *executable { b"\0x\0" } else { b"\0-\0" });
        listing.update(hex(&hasher.finalize()).as_bytes());
        listing.update(b"\n");
    }
    Ok(Tree {
        digest: format!("sha256:{}", hex(&listing.finalize())),
        bytes,
        files: files.len(),
        description,
    })
}

#[derive(Deserialize)]
struct Front {
    name: Option<String>,
    description: Option<String>,
}

fn frontmatter(text: &str) -> Result<Front, String> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or("SKILL.md does not start with a YAML frontmatter (---)")?;
    let end = rest
        .find("\n---")
        .ok_or("SKILL.md's frontmatter has no closing ---")?;
    serde_saphyr::from_str(&rest[..end]).map_err(|e| format!("SKILL.md's frontmatter: {e}"))
}

/// Collects `dir`'s files relative to `root`, with whether each is
/// executable, refusing anything but regular files and directories and
/// stopping at the first limit exceeded. `skip_git` leaves out a top-level
/// `.git` (a fetched checkout's).
fn walk(
    root: &Path,
    dir: &Path,
    skip_git: bool,
    files: &mut Vec<(PathBuf, bool)>,
    bytes: &mut u64,
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        if skip_git && dir == root && entry.file_name() == ".git" {
            continue;
        }
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| format!("cannot read {}: {e}", relative.display()))?;
        if meta.is_dir() {
            walk(root, &path, skip_git, files, bytes)?;
        } else if meta.is_file() {
            *bytes += meta.len();
            if *bytes > MAX_SKILL_BYTES {
                return Err(format!(
                    "it is over the limit of {} MiB",
                    MAX_SKILL_BYTES / (1024 * 1024)
                ));
            }
            files.push((relative, meta.permissions().mode() & 0o111 != 0));
            if files.len() > MAX_SKILL_FILES {
                return Err(format!("it has over {MAX_SKILL_FILES} files"));
            }
        } else {
            return Err(format!(
                "{} is not a regular file or directory (symlinks, devices and FIFOs are refused)",
                relative.display()
            ));
        }
    }
    Ok(())
}

/// Copies the skill at `from` to `to` (new), with the same checks as
/// [`walk`], so an oversized or unusual source costs at most the limit.
fn copy_checked(from: &Path, to: &Path, skip_git: bool) -> Result<(), String> {
    let meta = fs::symlink_metadata(from)
        .map_err(|e| format!("{} cannot be read: {e}", from.display()))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", from.display()));
    }
    let mut files = Vec::new();
    let mut bytes = 0;
    walk(from, from, skip_git, &mut files, &mut bytes)?;
    fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    for (relative, executable) in files {
        let target = to.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let mut src = fs::File::open(from.join(&relative))
            .map_err(|e| format!("cannot read {}: {e}", relative.display()))?;
        let mut dst = fs::File::create(&target)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
        // At most the limit, even if the file grew since the walk.
        let copied = io::copy(&mut (&mut src).take(MAX_SKILL_BYTES + 1), &mut dst)
            .map_err(|e| format!("cannot copy {}: {e}", relative.display()))?;
        if copied > MAX_SKILL_BYTES {
            return Err(format!("{} grew while it was copied", relative.display()));
        }
        dst.flush().map_err(|e| e.to_string())?;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&target, fs::Permissions::from_mode(mode))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The directory of a `path` skill: a project's relative to its repository
/// (and still inside it once symlinks are resolved), the machine's or the
/// user's absolute or under `~/`.
fn source_dir(path: &str, source: ToolSource, project: Option<&Path>) -> Result<PathBuf, String> {
    match source {
        ToolSource::Project => {
            let root = project.ok_or("a project skill needs the project's directory")?;
            let root = root
                .canonicalize()
                .map_err(|e| format!("{}: {e}", root.display()))?;
            let dir = root
                .join(path)
                .canonicalize()
                .map_err(|e| format!("{path}: {e}"))?;
            if !dir.starts_with(&root) {
                return Err(format!("{path} leaves the repository"));
            }
            Ok(dir)
        }
        ToolSource::Machine | ToolSource::User => {
            if let Some(rest) = path.strip_prefix("~/") {
                let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
                Ok(PathBuf::from(home).join(rest))
            } else if Path::new(path).is_absolute() {
                Ok(PathBuf::from(path))
            } else {
                Err(format!(
                    "{path}: a machine or user skill's path is absolute or starts with ~/"
                ))
            }
        }
    }
}

/// Fetches commit `rev` of `url` into `dir`: a shallow, blobless fetch of
/// that one commit, then a checkout of `subdir` (or everything), so only the
/// skill's files are downloaded. The user's Git configuration and credential
/// helpers apply; nothing is asked on a terminal.
fn fetch(url: &str, rev: &str, subdir: Option<&str>, dir: &Path) -> Result<(), String> {
    let git = |args: &[&str]| -> Result<String, String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(format!(
                "git {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    git(&["init", "-q"])?;
    git(&["fetch", "-q", "--depth=1", "--filter=blob:none", url, rev])?;
    let fetched = git(&["rev-parse", "FETCH_HEAD"])?;
    if !fetched.eq_ignore_ascii_case(rev) {
        return Err(format!("the repository gave commit {fetched}, not {rev}"));
    }
    git(&["checkout", "-q", "FETCH_HEAD", "--", subdir.unwrap_or(".")])?;
    Ok(())
}

/// Files 0444 (0555 if executable), directories 0555.
fn read_only(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            read_only(&path)?;
        } else {
            let mode = if meta.permissions().mode() & 0o111 != 0 {
                0o555
            } else {
                0o444
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        }
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o555))
}

fn make_writable(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            make_writable(&path)?;
        }
    }
    Ok(())
}

/// Where sessions' skills directories are, under the user's data directory.
pub const SESSIONS_DIR: &str = "agentux/sessions";

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
}

/// A session's skills: the directory to add to its workspace roots, Claude
/// Code's plugin option, OpenCode's configuration and the skill index. Notes
/// for the run's log say which skills it got, from where and by which
/// digest, and which it goes without and why.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Delivery {
    /// The session's skills directory, if it got any skill.
    pub dir: Option<PathBuf>,
    pub additional_directories: Vec<PathBuf>,
    pub meta: Option<serde_json::Map<String, serde_json::Value>>,
    /// Variables to start the harness with (OpenCode's configuration).
    pub env: Vec<(String, String)>,
    /// The skill index, for the session prompt.
    pub prompt: Option<String>,
    pub notes: Vec<String>,
}

/// How a harness gets the catalog's skills (ADR 0026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Claude Code: a local plugin, `agentux`, with only the skills in it,
    /// through the adapter's `_meta.claudeCode.options.plugins`.
    Plugin,
    /// Codex: `<dir>/.agents/skills`, which `codex-acp` takes from
    /// `additionalDirectories`.
    AgentsDir,
    /// OpenCode: `<dir>/.agents/skills` in `skills.paths`, through
    /// `OPENCODE_CONFIG_CONTENT` ([`opencode_config`]).
    OpenCodePaths,
    /// Any other harness, or one whose route failed: the skill index in the
    /// session prompt, over `<dir>/.agents/skills` ([`index`]).
    Index,
}

fn route(harness: &str) -> Route {
    match harness {
        "claude-code" => Route::Plugin,
        "codex" => Route::AgentsDir,
        "opencode" => Route::OpenCodePaths,
        _ => Route::Index,
    }
}

/// The catalog's skills for `harness`'s session `session_id`, copied from
/// the store into `~/.local/share/agentux/sessions/<session_id>`; `project`
/// is the run's worktree, which a project catalog's paths are relative to,
/// and `harness_env` the variables the harness gets over agentuxd's own
/// (its sandbox's).
pub fn deliver(
    catalog: &agentux_config::ToolCatalog,
    harness: &str,
    session_id: &str,
    project: &Path,
    harness_env: &[(String, String)],
) -> Delivery {
    let skills: Vec<&Skill> = catalog
        .skills
        .values()
        .filter(|s| s.applies_to(harness))
        .collect();
    if skills.is_empty() {
        return Delivery::default();
    }
    let (Some(store), Some(home)) = (Store::user(), data_home()) else {
        return Delivery {
            notes: vec!["skills: no data directory (HOME is not set); the session goes without the catalog's skills".into()],
            ..Delivery::default()
        };
    };
    deliver_with(
        &store,
        &home.join(SESSIONS_DIR).join(session_id),
        harness,
        skills,
        project,
        &harness_var(harness_env),
    )
}

/// A variable of the harness's environment: `harness_env`'s, else agentuxd's.
fn harness_var(harness_env: &[(String, String)]) -> impl Fn(&str) -> Option<String> + '_ {
    move |name: &str| {
        harness_env
            .iter()
            .find(|(var, _)| var == name)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
    }
}

/// What a harness's TUI is started with to get the skills of session
/// `session_id` when the session is handed over to it (ADRs 0007 and 0026).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tui {
    /// Arguments after the TUI's resume command.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The session's skills directory, to mount read-only in a sandbox.
    pub dir: Option<PathBuf>,
    /// Why the TUI goes without the skills, to show in the terminal.
    pub note: Option<String>,
}

/// [`Tui`] for `harness`'s session `session_id` in `cwd`, while its skills
/// directory lasts (it goes when the session ends); `harness_env` as for
/// [`deliver`].
pub fn tui(harness: &str, session_id: &str, cwd: &Path, harness_env: &[(String, String)]) -> Tui {
    match data_home() {
        Some(home) => tui_with(
            &home.join(SESSIONS_DIR).join(session_id),
            harness,
            cwd,
            &harness_var(harness_env),
        ),
        None => Tui::default(),
    }
}

/// [`tui`] with a given session directory.
fn tui_with(dir: &Path, harness: &str, cwd: &Path, var: &dyn Fn(&str) -> Option<String>) -> Tui {
    if !dir.is_dir() {
        return Tui::default();
    }
    let without = |why: String| Tui {
        note: Some(format!(
            "{harness}'s TUI goes without the catalog's skills: {why}."
        )),
        ..Tui::default()
    };
    match route(harness) {
        // The same plugin as the ACP session's.
        Route::Plugin if dir.join("agentux").is_dir() => Tui {
            args: vec![
                "--plugin-dir".into(),
                dir.join("agentux").to_string_lossy().into_owned(),
            ],
            dir: Some(dir.to_path_buf()),
            ..Tui::default()
        },
        Route::OpenCodePaths => match opencode_config(&dir.join(".agents/skills"), cwd, var) {
            Ok(content) => Tui {
                env: vec![("OPENCODE_CONFIG_CONTENT".into(), content)],
                dir: Some(dir.to_path_buf()),
                ..Tui::default()
            },
            Err(e) => without(e),
        },
        Route::AgentsDir => {
            without("`codex resume` has no verified way to add a skills directory".into())
        }
        Route::Plugin | Route::Index => Tui::default(),
    }
}

/// [`deliver`] with a given store and session directory, and `var` for the
/// harness's environment.
pub fn deliver_with(
    store: &Store,
    dir: &Path,
    harness: &str,
    mut skills: Vec<&Skill>,
    project: &Path,
    var: &dyn Fn(&str) -> Option<String>,
) -> Delivery {
    let mut out = Delivery::default();
    let mut route = route(harness);
    let skills_dir = match route {
        Route::Plugin => dir.join("agentux/skills"),
        _ => dir.join(".agents/skills"),
    };
    if route == Route::OpenCodePaths {
        match opencode_config(&skills_dir, project, var) {
            Ok(content) => out.env.push(("OPENCODE_CONFIG_CONTENT".into(), content)),
            Err(e) => {
                out.notes.push(format!(
                    "skills: cannot add the skills to {harness}'s skills.paths: {e}; the session gets the skill index instead"
                ));
                route = Route::Index;
            }
        }
    }
    if route == Route::Index {
        skills.retain(|skill| {
            let without = skill.fallback == Some(agentux_config::SkillFallback::Without);
            if without {
                out.notes.push(format!(
                    "skills: `{}` from the {} catalog is unavailable: {harness} has no native route and the entry has `fallback: none`",
                    skill.name, skill.source
                ));
            }
            !without
        });
        if skills.is_empty() {
            return out;
        }
    }
    // The machine's first, then the user's, then the project's; by name.
    skills.sort_by(|a, b| a.source.cmp(&b.source).then_with(|| a.name.cmp(&b.name)));
    let _ = remove_tree(dir);
    if let Err(e) = fs::create_dir_all(dir) {
        out.notes.push(format!(
            "skills: cannot create {}: {e}; the session goes without them",
            dir.display()
        ));
        return out;
    }
    let mut total = 0;
    let mut delivered = 0;
    let mut listed = Vec::new();
    for skill in skills {
        let from = format!("`{}` from the {} catalog", skill.name, skill.source);
        if delivered == agentux_config::MAX_SESSION_SKILLS {
            out.notes.push(format!(
                "skills: {from} left out: a session gets at most {} skills",
                agentux_config::MAX_SESSION_SKILLS
            ));
            continue;
        }
        let stored = match store.resolve(skill, Some(project)) {
            Ok(stored) => stored,
            Err(e) => {
                out.notes.push(format!("skills: {from} left out: {e}"));
                continue;
            }
        };
        if total + stored.bytes > agentux_config::MAX_SESSION_SKILL_BYTES {
            out.notes.push(format!(
                "skills: {from} left out: the session's skills would be over {} MiB",
                agentux_config::MAX_SESSION_SKILL_BYTES / (1024 * 1024)
            ));
            continue;
        }
        if let Err(e) = copy_checked(&stored.dir, &skills_dir.join(&skill.name), false) {
            out.notes.push(format!("skills: {from} left out: {e}"));
            continue;
        }
        total += stored.bytes;
        delivered += 1;
        let origin = match (&skill.git, &skill.rev) {
            (Some(git), Some(rev)) => format!("{git} at {}", &rev[..rev.len().min(12)]),
            _ => skill.path.clone().unwrap_or_default(),
        };
        let how = match route {
            Route::Plugin => "plugin agentux",
            Route::AgentsDir => ".agents/skills",
            Route::OpenCodePaths => "skills.paths",
            Route::Index => "the skill index",
        };
        out.notes.push(format!(
            "skills: {from} ({origin}, {}) for {harness}, through {how}",
            stored.digest
        ));
        listed.push((skill.name.clone(), stored.description));
    }
    if delivered == 0 {
        let _ = remove_tree(dir);
        out.env.clear();
        return out;
    }
    if route == Route::Index {
        out.prompt = Some(index(&skills_dir, &listed));
    }
    if route == Route::Plugin {
        let plugin = dir.join("agentux");
        let manifest = serde_json::json!({
            "name": "agentux",
            "description": "Skills from the AgentUX tool catalog",
            "version": "1.0.0",
        });
        let written = fs::create_dir_all(plugin.join(".claude-plugin")).and_then(|()| {
            fs::write(
                plugin.join(".claude-plugin/plugin.json"),
                serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
            )
        });
        if let Err(e) = written {
            out.notes.push(format!(
                "skills: cannot write the agentux plugin: {e}; the session goes without them"
            ));
            let _ = remove_tree(dir);
            return out;
        }
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".into(),
            serde_json::json!({ "options": { "plugins": [ { "type": "local", "path": plugin } ] } }),
        );
        out.meta = Some(meta);
    }
    let _ = read_only(dir);
    out.additional_directories = vec![dir.to_path_buf()];
    out.dir = Some(dir.to_path_buf());
    out
}

/// Where a skill index cuts each description when the whole index would be
/// over the instruction budget.
const INDEX_DESCRIPTION_CUT: usize = 256;

/// The skill index (ADR 0026): one line per skill in `skills_dir`, with its
/// description and the relative path of its `SKILL.md`, for the session
/// prompt. Over 16 KiB, each description is cut to its first 256 bytes at a
/// word boundary.
fn index(skills_dir: &Path, skills: &[(String, String)]) -> String {
    let render = |cut: bool| {
        let mut text = format!(
            "## Skills from the AgentUX tool catalog\n\n\
             These skills are in `{}`. When a task matches a skill's description, \
             read its `SKILL.md` and follow it before you start; the files it \
             mentions are next to it.\n",
            skills_dir.display()
        );
        for (name, description) in skills {
            let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
            let description = if cut {
                cut_at_word(&description, INDEX_DESCRIPTION_CUT)
            } else {
                description
            };
            text.push_str(&format!("\n- `{name}`: {description} (`{name}/SKILL.md`)"));
        }
        text
    };
    let full = render(false);
    if full.len() <= agentux_config::MAX_INSTRUCTION_BYTES {
        full
    } else {
        render(true)
    }
}

/// `text`'s first `max` bytes, ending at a word boundary, with `…` when cut.
fn cut_at_word(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let head = &text[..end];
    let head = head.rfind(' ').map_or(head, |space| &head[..space]);
    format!("{}…", head.trim_end())
}

/// `OPENCODE_CONFIG_CONTENT` for an OpenCode session whose skills are in
/// `skills_dir`, started in `cwd` (the worktree), with `var` for its
/// environment.
///
/// OpenCode 1.18.34 merges its configuration layers with remeda's
/// `mergeDeep`, which replaces arrays, and this variable comes after the
/// global and project layers (`packages/opencode/src/config/config.ts`), so
/// a bare `skills.paths` would drop theirs. The value is the one OpenCode
/// would use without the skills (the variable's own `skills.paths`, else the
/// last earlier layer's) with `skills_dir` added, and the rest of a value
/// agentuxd was started with is kept. The managed configuration
/// (`/etc/opencode`) and an account's organization configuration come later
/// and still replace it.
fn opencode_config(
    skills_dir: &Path,
    cwd: &Path,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut content = match var("OPENCODE_CONFIG_CONTENT") {
        Some(text) => {
            parse_jsonc(&text).map_err(|e| format!("OPENCODE_CONFIG_CONTENT is not JSON: {e}"))?
        }
        None => serde_json::json!({}),
    };
    let Some(config) = content.as_object_mut() else {
        return Err("OPENCODE_CONFIG_CONTENT is not a JSON object".into());
    };
    let mut paths = match config.get("skills").and_then(|s| s.get("paths")) {
        Some(paths) => string_list(paths)
            .ok_or("OPENCODE_CONFIG_CONTENT's skills.paths is not a list of strings")?,
        None => {
            let mut last = Vec::new();
            for file in opencode_layers(cwd, var) {
                let text = match fs::read_to_string(&file) {
                    Ok(text) => text,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
                };
                let layer = parse_jsonc(&text)
                    .map_err(|e| format!("{} is not JSON: {e}", file.display()))?;
                if let Some(paths) = layer.get("skills").and_then(|s| s.get("paths")) {
                    last = string_list(paths).ok_or_else(|| {
                        format!("{}'s skills.paths is not a list of strings", file.display())
                    })?;
                }
            }
            last
        }
    };
    let ours = skills_dir.to_string_lossy().into_owned();
    if !paths.contains(&ours) {
        paths.push(ours);
    }
    let skills = config
        .entry("skills")
        .or_insert_with(|| serde_json::json!({}));
    let Some(skills) = skills.as_object_mut() else {
        return Err("OPENCODE_CONFIG_CONTENT's skills is not an object".into());
    };
    skills.insert("paths".into(), serde_json::json!(paths));
    Ok(content.to_string())
}

/// The configuration files OpenCode 1.18.34 merges before
/// `OPENCODE_CONFIG_CONTENT`, in its order (`config/config.ts` and
/// `config/paths.ts`), for a session started at the worktree's root `cwd`.
fn opencode_layers(cwd: &Path, var: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let flag = |name: &str| var(name).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let home = var("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let mut files = Vec::new();
    let config_home = var("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    if let Some(dir) = config_home {
        for file in ["config.json", "opencode.json", "opencode.jsonc"] {
            files.push(dir.join("opencode").join(file));
        }
    }
    if let Some(file) = var("OPENCODE_CONFIG").filter(|f| !f.is_empty()) {
        files.push(cwd.join(file));
    }
    let project = !flag("OPENCODE_DISABLE_PROJECT_CONFIG");
    if project {
        files.push(cwd.join("opencode.json"));
        files.push(cwd.join("opencode.jsonc"));
    }
    let mut dirs = Vec::new();
    if project {
        dirs.push(cwd.join(".opencode"));
    }
    if let Some(home) = &home {
        dirs.push(home.join(".opencode"));
    }
    if let Some(dir) = var("OPENCODE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        dirs.push(cwd.join(dir));
    }
    let mut seen = Vec::new();
    for dir in dirs {
        if !seen.contains(&dir) {
            files.push(dir.join("opencode.json"));
            files.push(dir.join("opencode.jsonc"));
            seen.push(dir);
        }
    }
    files
}

fn string_list(value: &serde_json::Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect()
}

/// JSON with comments and trailing commas, as OpenCode reads its
/// configuration.
fn parse_jsonc(text: &str) -> Result<serde_json::Value, String> {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    let mut in_string = false;
    // Where the last comma went, while only blanks followed it.
    let mut comma: Option<usize> = None;
    while let Some(c) = chars.next() {
        if in_string {
            plain.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    plain.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '/' if chars.peek() == Some(&'/') => while chars.next_if(|&n| n != '\n').is_some() {},
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                plain.push(' ');
            }
            '}' | ']' => {
                if let Some(at) = comma.take() {
                    plain.replace_range(at..at + 1, " ");
                }
                plain.push(c);
            }
            ',' => {
                comma = Some(plain.len());
                plain.push(c);
            }
            c if c.is_whitespace() => plain.push(c),
            _ => {
                comma = None;
                in_string = c == '"';
                plain.push(c);
            }
        }
    }
    serde_json::from_str(&plain).map_err(|e| e.to_string())
}

/// Removes session `session_id`'s skills directory, if it has one.
pub fn remove_session_dir(session_id: &str) {
    if let Some(home) = data_home() {
        let _ = remove_tree(&home.join(SESSIONS_DIR).join(session_id));
    }
}

/// Removes a read-only tree.
fn remove_tree(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    make_writable(dir)?;
    fs::remove_dir_all(dir)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir(root: &Path, name: &str, body: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Writes release notes.\n---\n{body}\n"),
        )
        .unwrap();
        fs::write(dir.join("scripts/run.sh"), "echo hi\n").unwrap();
        fs::set_permissions(
            dir.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        dir
    }

    fn entry(name: &str, path: &str, source: ToolSource) -> Skill {
        Skill {
            name: name.into(),
            path: Some(path.into()),
            git: None,
            rev: None,
            subdir: None,
            digest: None,
            harnesses: None,
            fallback: None,
            locked: false,
            source,
        }
    }

    #[test]
    fn the_digest_covers_paths_contents_and_the_exec_bit_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = skill_dir(tmp.path(), "notes", "Use the changelog.");
        let first = check(&dir, Some("notes")).unwrap();
        assert!(first.digest.starts_with("sha256:") && first.digest.len() == 71);
        assert_eq!(first.files, 2);
        assert_eq!(first.description, "Writes release notes.");

        // Times and other modes don't count.
        fs::set_permissions(dir.join("SKILL.md"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(check(&dir, Some("notes")).unwrap().digest, first.digest);
        // The exec bit does.
        fs::set_permissions(
            dir.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_ne!(check(&dir, Some("notes")).unwrap().digest, first.digest);
        // So does content.
        fs::set_permissions(
            dir.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(dir.join("scripts/run.sh"), "echo bye\n").unwrap();
        assert_ne!(check(&dir, Some("notes")).unwrap().digest, first.digest);
    }

    #[test]
    fn a_skill_outside_the_rules_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = skill_dir(tmp.path(), "notes", "x");
        assert!(
            check(&dir, Some("other"))
                .unwrap_err()
                .contains("names the skill `notes`")
        );

        std::os::unix::fs::symlink("/etc/passwd", dir.join("link")).unwrap();
        assert!(
            check(&dir, None)
                .unwrap_err()
                .contains("not a regular file")
        );
        fs::remove_file(dir.join("link")).unwrap();

        fs::write(dir.join("SKILL.md"), "no frontmatter").unwrap();
        assert!(check(&dir, None).unwrap_err().contains("frontmatter"));
        fs::write(dir.join("SKILL.md"), "---\nname: notes\n---\nbody").unwrap();
        assert!(check(&dir, None).unwrap_err().contains("no description"));

        let big = format!(
            "---\ndescription: big\n---\n{}",
            "x".repeat(MAX_SKILL_MD_BYTES as usize)
        );
        fs::write(dir.join("SKILL.md"), big).unwrap();
        assert!(check(&dir, None).unwrap_err().contains("SKILL.md is"));

        let many = skill_dir(tmp.path(), "many", "x");
        for i in 0..MAX_SKILL_FILES {
            fs::write(many.join(format!("f{i}")), "").unwrap();
        }
        assert!(check(&many, None).unwrap_err().contains("over 1000 files"));
    }

    #[test]
    fn a_path_skill_is_stored_read_only_under_its_digest_once() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        skill_dir(&repo.join("skills"), "notes", "Use the changelog.");
        let store = Store::new(tmp.path().join("store"));

        let stored = store
            .resolve(
                &entry("notes", "skills/notes", ToolSource::Project),
                Some(&repo),
            )
            .unwrap();
        assert_eq!(stored.dir, store.dir_of(&stored.digest));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&stored.dir.join("SKILL.md")), 0o444);
        assert_eq!(mode(&stored.dir.join("scripts/run.sh")), 0o555);
        assert_eq!(mode(&stored.dir), 0o555);
        assert_eq!(
            check(&stored.dir, Some("notes")).unwrap().digest,
            stored.digest
        );

        // Again: the same entry, nothing new, no temporary directory left.
        let again = store
            .resolve(
                &entry("notes", "skills/notes", ToolSource::Project),
                Some(&repo),
            )
            .unwrap();
        assert_eq!(again, stored);
        let left: Vec<_> = fs::read_dir(store.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            [stored
                .dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()]
        );

        // A digest that doesn't match is refused.
        let mut pinned = entry("notes", "skills/notes", ToolSource::Project);
        pinned.digest = Some(format!("sha256:{}", "0".repeat(64)));
        assert!(
            store
                .resolve(&pinned, Some(&repo))
                .unwrap_err()
                .contains("does not match")
        );

        // A project path that leaves the repository through a symlink is refused.
        std::os::unix::fs::symlink(tmp.path(), repo.join("out")).unwrap();
        let escaped = store
            .resolve(&entry("notes", "out", ToolSource::Project), Some(&repo))
            .unwrap_err();
        assert!(escaped.contains("leaves the repository"), "{escaped}");
    }

    #[test]
    fn a_git_skill_is_fetched_from_its_commit() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        skill_dir(&origin.join("skills"), "notes", "From git.");
        fs::write(origin.join("big.bin"), vec![0u8; 1024]).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&origin)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["config", "uploadpack.allowFilter", "true"]);
        git(&["config", "uploadpack.allowAnySHA1InWant", "true"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "skills"]);
        let rev = git(&["rev-parse", "HEAD"]);

        let store = Store::new(tmp.path().join("store"));
        let mut skill = entry("notes", "", ToolSource::User);
        skill.path = None;
        skill.git = Some(format!("file://{}", origin.display()));
        skill.rev = Some(rev.clone());
        skill.subdir = Some("skills/notes".into());
        let stored = store.resolve(&skill, None).unwrap();
        assert_eq!(stored.files, 2, "only the subdir, without .git");
        assert_eq!(stored.description, "Writes release notes.");

        skill.rev = Some("0".repeat(40));
        assert!(store.resolve(&skill, None).is_err());
    }

    /// A harness environment without any variable.
    fn no_vars(_: &str) -> Option<String> {
        None
    }

    fn delivered(tmp: &Path, harness: &str, names: &[&str]) -> (Delivery, PathBuf) {
        let entries: Vec<Skill> = names
            .iter()
            .map(|name| entry(name, &format!("skills/{name}"), ToolSource::Project))
            .collect();
        delivered_entries(tmp, harness, &entries, &no_vars)
    }

    fn delivered_entries(
        tmp: &Path,
        harness: &str,
        entries: &[Skill],
        var: &dyn Fn(&str) -> Option<String>,
    ) -> (Delivery, PathBuf) {
        let repo = tmp.join("repo");
        for skill in entries {
            skill_dir(&repo.join("skills"), &skill.name, "Body.");
        }
        let store = Store::new(tmp.join("store"));
        let dir = tmp.join("sessions/s1");
        (
            deliver_with(&store, &dir, harness, entries.iter().collect(), &repo, var),
            dir,
        )
    }

    #[test]
    fn claude_code_gets_an_agentux_plugin_with_only_the_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let (out, dir) = delivered(tmp.path(), "claude-code", &["notes", "pdf"]);
        assert_eq!(out.dir.as_deref(), Some(dir.as_path()));
        assert_eq!(out.additional_directories, std::slice::from_ref(&dir));
        let plugin = dir.join("agentux");
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(plugin.join(".claude-plugin/plugin.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["name"], "agentux");
        assert!(plugin.join("skills/notes/SKILL.md").is_file());
        assert!(plugin.join("skills/pdf/scripts/run.sh").is_file());
        let mut top: Vec<_> = fs::read_dir(&plugin)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        top.sort();
        assert_eq!(
            top,
            [".claude-plugin", "skills"],
            "no hooks, agents, commands or MCP servers"
        );
        assert!(!dir.join(".claude").exists(), "nothing loads twice");
        let meta = serde_json::Value::Object(out.meta.unwrap());
        assert_eq!(meta["claudeCode"]["options"]["plugins"][0]["type"], "local");
        assert_eq!(
            meta["claudeCode"]["options"]["plugins"][0]["path"],
            plugin.to_string_lossy().as_ref()
        );
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("`notes` from the project catalog") && n.contains("sha256:"))
        );
        let mode = fs::metadata(plugin.join("skills/notes/SKILL.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o222, 0, "read-only");

        remove_tree(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn codex_gets_agents_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let (out, dir) = delivered(tmp.path(), "codex", &["notes"]);
        assert!(dir.join(".agents/skills/notes/SKILL.md").is_file());
        assert_eq!(out.meta, None);
        assert!(out.env.is_empty() && out.prompt.is_none());
        assert_eq!(out.additional_directories, [dir]);
    }

    fn content(out: &Delivery) -> serde_json::Value {
        let [(var, value)] = out.env.as_slice() else {
            panic!("{:?}", out.env);
        };
        assert_eq!(var, "OPENCODE_CONFIG_CONTENT");
        serde_json::from_str(value).unwrap()
    }

    #[test]
    fn opencode_gets_agents_skills_in_its_skills_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (out, dir) = delivered(tmp.path(), "opencode", &["notes"]);
        assert!(dir.join(".agents/skills/notes/SKILL.md").is_file());
        assert_eq!(out.additional_directories, std::slice::from_ref(&dir));
        assert_eq!(out.prompt, None);
        assert_eq!(
            content(&out),
            serde_json::json!({"skills": {"paths": [dir.join(".agents/skills")]}})
        );
        assert!(out.notes.iter().any(|n| n.contains("through skills.paths")));
    }

    #[test]
    fn opencode_keeps_the_skills_paths_it_would_have_used() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        let write = |path: &Path, text: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        let ours = tmp.path().join("sessions/s1/.agents/skills");
        let paths = |out: &Delivery| content(out)["skills"]["paths"].clone();

        // The user's global paths, which a project file without paths keeps.
        write(
            &home.join(".config/opencode/opencode.json"),
            r#"{"skills": {"paths": ["~/my-skills"]}}"#,
        );
        write(&repo.join("opencode.json"), r#"{"model": "x/y"}"#);
        let var = |name: &str| (name == "HOME").then(|| home.to_string_lossy().into_owned());
        let (out, _) = delivered_entries(
            tmp.path(),
            "opencode",
            &[entry("notes", "skills/notes", ToolSource::Project)],
            &var,
        );
        assert_eq!(paths(&out), serde_json::json!(["~/my-skills", ours]));

        // A later layer replaces the array, as OpenCode's merge does; JSONC is
        // read with comments and trailing commas.
        write(
            &repo.join(".opencode/opencode.jsonc"),
            "// project\n{\"skills\": {\"paths\": [\"team/skills\", /* ok */],},}\n",
        );
        let (out, _) = delivered_entries(
            tmp.path(),
            "opencode",
            &[entry("notes", "skills/notes", ToolSource::Project)],
            &var,
        );
        assert_eq!(paths(&out), serde_json::json!(["team/skills", ours]));

        // The sandbox's config home replaces the user's.
        let state = tmp.path().join("state");
        write(
            &state.join("config/opencode/config.json"),
            r#"{"skills": {"paths": ["/sandboxed"]}}"#,
        );
        fs::remove_file(repo.join(".opencode/opencode.jsonc")).unwrap();
        let sandboxed = |name: &str| match name {
            "HOME" => Some(home.to_string_lossy().into_owned()),
            "XDG_CONFIG_HOME" => Some(state.join("config").to_string_lossy().into_owned()),
            _ => None,
        };
        let (out, _) = delivered_entries(
            tmp.path(),
            "opencode",
            &[entry("notes", "skills/notes", ToolSource::Project)],
            &sandboxed,
        );
        assert_eq!(paths(&out), serde_json::json!(["/sandboxed", ours]));

        // agentuxd's own OPENCODE_CONFIG_CONTENT wins over the files and
        // keeps its other settings.
        let given = |name: &str| match name {
            "HOME" => Some(home.to_string_lossy().into_owned()),
            "OPENCODE_CONFIG_CONTENT" => Some(
                r#"{"model": "a/b", "skills": {"paths": ["/given"], "urls": ["https://s"]}}"#
                    .into(),
            ),
            _ => None,
        };
        let (out, _) = delivered_entries(
            tmp.path(),
            "opencode",
            &[entry("notes", "skills/notes", ToolSource::Project)],
            &given,
        );
        assert_eq!(
            content(&out),
            serde_json::json!({"model": "a/b", "skills": {"paths": ["/given", ours], "urls": ["https://s"]}})
        );
    }

    #[test]
    fn opencode_gets_the_index_when_its_config_cannot_be_merged() {
        let tmp = tempfile::tempdir().unwrap();
        let var = |name: &str| (name == "OPENCODE_CONFIG_CONTENT").then(|| "{not json".to_string());
        let (out, dir) = delivered_entries(
            tmp.path(),
            "opencode",
            &[entry("notes", "skills/notes", ToolSource::Project)],
            &var,
        );
        assert!(out.env.is_empty());
        assert!(
            out.notes[0].contains("OPENCODE_CONFIG_CONTENT is not JSON"),
            "{:?}",
            out.notes
        );
        assert!(out.notes[0].ends_with("the session gets the skill index instead"));
        let prompt = out.prompt.unwrap();
        assert!(prompt.contains(&dir.join(".agents/skills").display().to_string()));
        assert!(
            prompt.contains("- `notes`: Writes release notes. (`notes/SKILL.md`)"),
            "{prompt}"
        );
    }

    #[test]
    fn antigravity_gets_the_skill_index_without_fallback_none_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hidden = entry("secret", "skills/secret", ToolSource::Project);
        hidden.fallback = Some(agentux_config::SkillFallback::Without);
        let entries = [entry("notes", "skills/notes", ToolSource::Project), hidden];
        let (out, dir) = delivered_entries(tmp.path(), "antigravity", &entries, &no_vars);
        assert!(dir.join(".agents/skills/notes/SKILL.md").is_file());
        assert!(!dir.join(".agents/skills/secret").exists());
        assert_eq!(out.additional_directories, [dir]);
        let prompt = out.prompt.unwrap();
        assert!(prompt.starts_with("## Skills from the AgentUX tool catalog"));
        assert!(
            prompt.contains("`notes`") && !prompt.contains("`secret`"),
            "{prompt}"
        );
        assert!(out.notes.iter().any(|n| {
            n.contains("`secret` from the project catalog is unavailable")
                && n.contains("`fallback: none`")
        }));
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("through the skill index"))
        );

        // Only `fallback: none` entries: nothing at all.
        let tmp = tempfile::tempdir().unwrap();
        let mut only = entry("secret", "skills/secret", ToolSource::Project);
        only.fallback = Some(agentux_config::SkillFallback::Without);
        let (out, dir) = delivered_entries(tmp.path(), "antigravity", &[only], &no_vars);
        assert!(out.prompt.is_none() && out.dir.is_none() && !dir.exists());

        // A native route ignores `fallback`.
        let tmp = tempfile::tempdir().unwrap();
        let mut native = entry("secret", "skills/secret", ToolSource::Project);
        native.fallback = Some(agentux_config::SkillFallback::Without);
        let (_, dir) = delivered_entries(tmp.path(), "codex", &[native], &no_vars);
        assert!(dir.join(".agents/skills/secret/SKILL.md").is_file());
    }

    #[test]
    fn a_handed_over_tui_gets_the_sessions_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");

        let (_, dir) = delivered(tmp.path(), "claude-code", &["notes"]);
        let claude = tui_with(&dir, "claude-code", &repo, &no_vars);
        assert_eq!(
            claude.args,
            [
                "--plugin-dir".to_string(),
                dir.join("agentux").to_string_lossy().into_owned()
            ]
        );
        assert_eq!(claude.dir.as_deref(), Some(dir.as_path()));
        assert!(claude.env.is_empty() && claude.note.is_none());

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (_, dir) = delivered(tmp.path(), "opencode", &["notes"]);
        let opencode = tui_with(&dir, "opencode", &repo, &no_vars);
        assert!(opencode.args.is_empty());
        let [(var, value)] = opencode.env.as_slice() else {
            panic!("{:?}", opencode.env);
        };
        assert_eq!(var, "OPENCODE_CONFIG_CONTENT");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(value).unwrap(),
            serde_json::json!({"skills": {"paths": [dir.join(".agents/skills")]}})
        );

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let (_, dir) = delivered(tmp.path(), "codex", &["notes"]);
        let codex = tui_with(&dir, "codex", &repo, &no_vars);
        assert!(codex.args.is_empty() && codex.env.is_empty() && codex.dir.is_none());
        assert!(codex.note.unwrap().contains("codex resume"));

        // An ended session has no directory, and its TUI nothing to add.
        remove_tree(&dir).unwrap();
        assert_eq!(tui_with(&dir, "opencode", &repo, &no_vars), Tui::default());
    }

    #[test]
    fn a_long_index_cuts_descriptions_at_a_word() {
        let long = "word ".repeat(200);
        let skills: Vec<(String, String)> = (0..32)
            .map(|i| (format!("skill-{i:02}"), long.clone()))
            .collect();
        let text = index(Path::new("/s/.agents/skills"), &skills);
        assert!(
            text.len() <= agentux_config::MAX_INSTRUCTION_BYTES,
            "{}",
            text.len()
        );
        assert_eq!(
            text.matches("\n- `skill-").count(),
            32,
            "no skill is left out"
        );
        assert!(text.contains("word word…"));
        // A short one stays whole.
        let text = index(Path::new("/s"), &[("a".into(), "Does\n  a thing.".into())]);
        assert!(
            text.ends_with("\n- `a`: Does a thing. (`a/SKILL.md`)"),
            "{text}"
        );
        assert_eq!(cut_at_word("aé bc", 3), "aé…");
    }

    #[test]
    fn jsonc_keeps_strings_and_drops_comments_and_trailing_commas() {
        let value = parse_jsonc(
            "\u{feff}{\n // c\n \"a\": \"http://x/*y*/,]\", /* b */ \"b\": [1, 2,],\n}",
        )
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"a": "http://x/*y*/,]", "b": [1, 2]})
        );
        assert_eq!(parse_jsonc(r#"{"q": "a\"//b"}"#).unwrap()["q"], "a\"//b");
        assert!(parse_jsonc("{").is_err());
    }

    #[test]
    fn a_skill_that_fails_is_left_out_and_the_others_go_on() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        skill_dir(&repo.join("skills"), "good", "Body.");
        let entries = [
            entry("good", "skills/good", ToolSource::Project),
            entry("missing", "skills/missing", ToolSource::Project),
        ];
        let store = Store::new(tmp.path().join("store"));
        let dir = tmp.path().join("sessions/s2");
        let out = deliver_with(
            &store,
            &dir,
            "codex",
            entries.iter().collect(),
            &repo,
            &no_vars,
        );
        assert!(dir.join(".agents/skills/good").is_dir());
        assert!(!dir.join(".agents/skills/missing").exists());
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("skills: `missing` from the project catalog left out")),
            "{:?}",
            out.notes
        );

        // Nothing delivered: no directory at all.
        let only_bad = [entry("missing", "skills/missing", ToolSource::Project)];
        let empty = tmp.path().join("sessions/s3");
        let out = deliver_with(
            &store,
            &empty,
            "codex",
            only_bad.iter().collect(),
            &repo,
            &no_vars,
        );
        assert_eq!(out.dir, None);
        assert!(!empty.exists());
    }

    #[test]
    fn a_session_gets_at_most_32_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..33).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (out, dir) = delivered(tmp.path(), "codex", &refs);
        assert_eq!(
            fs::read_dir(dir.join(".agents/skills")).unwrap().count(),
            32
        );
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("`s32`") && n.contains("at most 32 skills"))
        );
    }
}
