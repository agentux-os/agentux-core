//! Detection of a project's lint and test commands, used to fill the gate of
//! the built-in default pipeline when a project has no `agentux.yaml`.
//!
//! Each check (`lint`, then `test`) takes its command from the first source
//! below that provides one. Only files at the project root are read.
//!
//! | Source | `lint` | `test` |
//! |---|---|---|
//! | `justfile` (also `Justfile`, `.justfile`) | `just lint`, if the recipe exists | `just test`, if the recipe exists |
//! | `Cargo.toml` | `cargo clippy --all-targets -- -D warnings` | `cargo test` |
//! | `package.json` | `<pm> run lint`, if the script exists | `<pm> run test`, if the script exists and is not npm's placeholder |
//! | `pyproject.toml` | `uv run ruff check`, if it has a `[tool.ruff]` table | `uv run pytest`, if pytest is configured |
//! | `go.mod` | `go vet ./...` | `go test ./...` |
//!
//! `<pm>` is `pnpm`, `yarn` or `bun` when its lockfile is present, otherwise
//! `npm`. Pytest counts as configured when `pyproject.toml` mentions `pytest`
//! (a `[tool.pytest.ini_options]` table or a dependency) or `pytest.ini`
//! exists. A justfile goes first because a project that has one has already
//! said how it wants to be checked.

use std::fs;
use std::path::Path;

use crate::Check;

/// Names of the checks the default pipeline gates on, in order.
const CHECK_NAMES: [&str; 2] = ["lint", "test"];

/// Detects the `lint` and `test` checks of the project at `root`. Returns
/// only the checks found, so the result may be empty.
pub fn detect_checks(root: &Path) -> Vec<Check> {
    let sources = [justfile, cargo, node, python, go];
    let found: Vec<[Option<String>; 2]> = sources.iter().map(|source| source(root)).collect();
    CHECK_NAMES
        .iter()
        .enumerate()
        .filter_map(|(i, name)| {
            let run = found.iter().find_map(|commands| commands[i].clone())?;
            Some(Check {
                name: (*name).to_string(),
                run,
            })
        })
        .collect()
}

/// `[lint, test]` commands offered by one source.
type Commands = [Option<String>; 2];

fn read(root: &Path, name: &str) -> Option<String> {
    fs::read_to_string(root.join(name)).ok()
}

fn justfile(root: &Path) -> Commands {
    let Some(text) = ["justfile", "Justfile", ".justfile"]
        .iter()
        .find_map(|name| read(root, name))
    else {
        return [None, None];
    };
    let recipes = just_recipes(&text);
    CHECK_NAMES.map(|name| {
        recipes
            .iter()
            .any(|recipe| recipe == name)
            .then(|| format!("just {name}"))
    })
}

/// Recipe names in a justfile: unindented lines of the form
/// `[@]name [params]: [deps]`. Assignments (`x := ...`), settings, aliases,
/// attributes and comments are skipped.
fn just_recipes(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| {
            let colon = line.find(':')?;
            if line[colon + 1..].starts_with('=') {
                return None;
            }
            let name = line[..colon].split_whitespace().next()?;
            let name = name.strip_prefix('@').unwrap_or(name);
            let valid = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            valid.then(|| name.to_string())
        })
        .collect()
}

fn cargo(root: &Path) -> Commands {
    if !root.join("Cargo.toml").is_file() {
        return [None, None];
    }
    [
        Some("cargo clippy --all-targets -- -D warnings".to_string()),
        Some("cargo test".to_string()),
    ]
}

fn node(root: &Path) -> Commands {
    let Some(text) = read(root, "package.json") else {
        return [None, None];
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
        return [None, None];
    };
    let pm = [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lock", "bun"),
        ("bun.lockb", "bun"),
    ]
    .iter()
    .find(|(lockfile, _)| root.join(lockfile).is_file())
    .map_or("npm", |(_, pm)| pm);
    let script = |name: &str| manifest["scripts"][name].as_str().map(str::to_string);
    CHECK_NAMES.map(|name| {
        let body = script(name)?;
        // `npm init` writes a test script that always fails.
        if name == "test" && body.contains("no test specified") {
            return None;
        }
        Some(format!("{pm} run {name}"))
    })
}

fn python(root: &Path) -> Commands {
    let Some(text) = read(root, "pyproject.toml") else {
        return [None, None];
    };
    let lint = text
        .contains("[tool.ruff")
        .then(|| "uv run ruff check".to_string());
    let pytest = text.contains("pytest") || root.join("pytest.ini").is_file();
    [lint, pytest.then(|| "uv run pytest".to_string())]
}

fn go(root: &Path) -> Commands {
    if !root.join("go.mod").is_file() {
        return [None, None];
    }
    [
        Some("go vet ./...".to_string()),
        Some("go test ./...".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in files {
            fs::write(dir.path().join(name), contents).unwrap();
        }
        dir
    }

    fn detected(files: &[(&str, &str)]) -> Vec<(String, String)> {
        let dir = project(files);
        detect_checks(dir.path())
            .into_iter()
            .map(|check| (check.name, check.run))
            .collect()
    }

    fn checks(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, run)| (name.to_string(), run.to_string()))
            .collect()
    }

    #[test]
    fn empty_project_has_no_checks() {
        assert_eq!(detected(&[]), []);
        assert_eq!(detected(&[("README.md", "# hi")]), []);
    }

    #[test]
    fn rust() {
        assert_eq!(
            detected(&[("Cargo.toml", "[package]\nname = \"x\"\n")]),
            checks(&[
                ("lint", "cargo clippy --all-targets -- -D warnings"),
                ("test", "cargo test")
            ])
        );
    }

    #[test]
    fn go_module() {
        assert_eq!(
            detected(&[("go.mod", "module example.com/x\n")]),
            checks(&[("lint", "go vet ./..."), ("test", "go test ./...")])
        );
    }

    #[test]
    fn node_uses_existing_scripts_and_the_lockfile_package_manager() {
        let manifest = r#"{"scripts": {"lint": "eslint .", "build": "tsc"}}"#;
        assert_eq!(
            detected(&[("package.json", manifest)]),
            checks(&[("lint", "npm run lint")])
        );
        let manifest = r#"{"scripts": {"lint": "eslint .", "test": "vitest"}}"#;
        assert_eq!(
            detected(&[("package.json", manifest), ("pnpm-lock.yaml", "")]),
            checks(&[("lint", "pnpm run lint"), ("test", "pnpm run test")])
        );
        assert_eq!(
            detected(&[
                ("package.json", r#"{"scripts": {"test": "jest"}}"#),
                ("yarn.lock", "")
            ]),
            checks(&[("test", "yarn run test")])
        );
    }

    #[test]
    fn node_skips_the_npm_init_placeholder_and_bad_json() {
        let manifest = r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#;
        assert_eq!(detected(&[("package.json", manifest)]), []);
        assert_eq!(detected(&[("package.json", "{not json")]), []);
        assert_eq!(detected(&[("package.json", "{}")]), []);
    }

    #[test]
    fn python_needs_pytest_configured() {
        assert_eq!(
            detected(&[("pyproject.toml", "[project]\nname = \"x\"\n")]),
            []
        );
        assert_eq!(
            detected(&[(
                "pyproject.toml",
                "[project]\nname = \"x\"\n[tool.pytest.ini_options]\n"
            )]),
            checks(&[("test", "uv run pytest")])
        );
        assert_eq!(
            detected(&[
                (
                    "pyproject.toml",
                    "[project]\n[tool.ruff]\nline-length = 100\n"
                ),
                ("pytest.ini", "[pytest]\n")
            ]),
            checks(&[("lint", "uv run ruff check"), ("test", "uv run pytest")])
        );
    }

    #[test]
    fn justfile_recipes_win_and_missing_ones_fall_through() {
        let justfile = "\
set shell := [\"bash\", \"-c\"]
version := \"1\"
alias t := test

# Run the linters
[group('ci')]
@lint:
    cargo clippy

test *args: lint
    cargo nextest run {{args}}
";
        assert_eq!(
            detected(&[("justfile", justfile), ("Cargo.toml", "")]),
            checks(&[("lint", "just lint"), ("test", "just test")])
        );
        // Only `test` is a recipe: lint comes from Cargo.toml.
        assert_eq!(
            detected(&[("Justfile", "test:\n    cargo test\n"), ("Cargo.toml", "")]),
            checks(&[
                ("lint", "cargo clippy --all-targets -- -D warnings"),
                ("test", "just test")
            ])
        );
        // A justfile without those recipes contributes nothing.
        assert_eq!(detected(&[("justfile", "build:\n    make\n")]), []);
    }

    #[test]
    fn recipe_parsing() {
        assert_eq!(
            just_recipes("a:\nb-c arg='x': a\n  indented:\n_d:\nx := 'y'\n# e:\n"),
            ["a", "b-c", "_d"]
        );
    }
}
