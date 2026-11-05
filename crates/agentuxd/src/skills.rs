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
}
