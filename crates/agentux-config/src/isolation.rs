//! Container isolation of check commands (ADR 0009), check timeouts, and the
//! dev container image lookup the default image comes from.

use std::path::Path;
use std::time::Duration;
use std::{fmt, fs, io};

use serde::Deserialize;

/// The image isolated checks run in when the project names none and has no
/// dev container image.
pub const DEFAULT_IMAGE: &str = "registry.fedoraproject.org/fedora-toolbox:44";

/// How long a check may run when its `timeout` is not set.
pub const DEFAULT_CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Where a dev container definition is looked for, relative to the project
/// root, in this order (the Dev Container specification's own order).
pub const DEVCONTAINER_FILES: [&str; 2] = [".devcontainer/devcontainer.json", ".devcontainer.json"];

/// Where gate steps run their check commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Isolation {
    pub mode: IsolationMode,
    /// The image to run checks in. `None`: the dev container's image, else
    /// [`DEFAULT_IMAGE`]; see [`Isolation::image`].
    pub image: Option<String>,
    /// Whether the container gets a network. Off by default.
    pub network: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// Checks run directly on the host, as the user running `agentuxd`.
    #[default]
    None,
    /// Checks run in a rootless Podman container.
    Podman,
}

impl IsolationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Podman => "podman",
        }
    }
}

impl fmt::Display for IsolationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The image isolated checks use, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub reference: String,
    pub source: ImageSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// `isolation.image` in `agentux.yaml`.
    Config,
    /// The `image` of the dev container definition at this path, relative to
    /// the project root.
    Devcontainer(&'static str),
    /// [`DEFAULT_IMAGE`]. `Some(path)`: a dev container definition exists
    /// there but has no `image` (it builds one, or uses Compose).
    Default(Option<&'static str>),
}

impl fmt::Display for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reference)?;
        match self.source {
            ImageSource::Config => f.write_str(" (from agentux.yaml)"),
            ImageSource::Devcontainer(path) => write!(f, " (from {path})"),
            ImageSource::Default(None) => f.write_str(" (default)"),
            ImageSource::Default(Some(path)) => {
                write!(f, " (default; {path} has no `image`)")
            }
        }
    }
}

impl Isolation {
    /// The image to run checks in. `read` returns the contents of a file
    /// relative to the project root, `Ok(None)` if it does not exist; the
    /// daemon reads the run's base commit, `aux validate` the working tree.
    ///
    /// An explicit `image` wins; then the `image` of the project's dev
    /// container definition; then [`DEFAULT_IMAGE`]. Only the `image` field is
    /// used: dev container builds, features and Compose files are not.
    pub fn image(
        &self,
        mut read: impl FnMut(&str) -> io::Result<Option<String>>,
    ) -> Result<Image, String> {
        if let Some(image) = &self.image {
            return Ok(Image {
                reference: image.clone(),
                source: ImageSource::Config,
            });
        }
        for path in DEVCONTAINER_FILES {
            let Some(text) = read(path).map_err(|e| format!("cannot read {path}: {e}"))? else {
                continue;
            };
            return match devcontainer_image(&text).map_err(|e| format!("{path}: {e}"))? {
                Some(reference) => {
                    image_issue(&reference).map_err(|e| format!("{path}: `image` {e}"))?;
                    Ok(Image {
                        reference,
                        source: ImageSource::Devcontainer(path),
                    })
                }
                None => Ok(Image {
                    reference: DEFAULT_IMAGE.to_string(),
                    source: ImageSource::Default(Some(path)),
                }),
            };
        }
        Ok(Image {
            reference: DEFAULT_IMAGE.to_string(),
            source: ImageSource::Default(None),
        })
    }

    /// [`Isolation::image`] for a project checked out at `root`.
    pub fn image_in(&self, root: &Path) -> Result<Image, String> {
        self.image(|path| read_optional(&root.join(path)))
    }
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The `image` of a `devcontainer.json`, which is JSON with comments and
/// trailing commas.
pub fn devcontainer_image(text: &str) -> Result<Option<String>, String> {
    let json = strip_jsonc(text);
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("not valid JSON: {e}"))?;
    match value.get("image") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(image)) => Ok(Some(image.clone())),
        Some(_) => Err("`image` must be a string".into()),
    }
}

/// Turns JSON with comments into JSON: `//` and `/* */` comments outside
/// strings become spaces (newlines are kept, so error positions still match)
/// and commas directly before `}` or `]` are dropped.
fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 1;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                out.push_str("  ");
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push_str("  ");
                    i += 2;
                }
            }
            (',', _) => {
                // A trailing comma: only whitespace and comments until `}` or `]`.
                let next = next_significant(&chars, i + 1);
                out.push(if matches!(next, Some('}' | ']')) {
                    ' '
                } else {
                    ','
                });
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The first character from `i` on that is not whitespace or in a comment.
fn next_significant(chars: &[char], mut i: usize) -> Option<char> {
    loop {
        match (chars.get(i)?, chars.get(i + 1)) {
            (c, _) if c.is_whitespace() => i += 1,
            ('/', Some('/')) => {
                while chars.get(i).is_some_and(|&c| c != '\n') {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            (&c, _) => return Some(c),
        }
    }
}

/// Why `image` cannot be passed to Podman as an image reference, if it
/// cannot. It must not look like an option, and must be one word.
pub(crate) fn image_issue(image: &str) -> Result<(), String> {
    if image.trim().is_empty() {
        Err("must not be empty".into())
    } else if image.starts_with('-') {
        Err("must not start with `-`".into())
    } else if image.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Err("must not contain spaces".into())
    } else {
        Ok(())
    }
}

/// Parses a timeout such as `90s`, `30m` or `2h`.
pub(crate) fn parse_timeout(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let seconds_per = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => {
            return Err(format!(
                "`{text}` is not a duration; use a whole number with s, m or h, e.g. 30m"
            ));
        }
    };
    let number: u64 = number.parse().map_err(|_| {
        format!("`{text}` is not a duration; use a whole number with s, m or h, e.g. 30m")
    })?;
    if number == 0 {
        return Err("must be longer than zero".into());
    }
    number
        .checked_mul(seconds_per)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}` is too long"))
}

/// Formats a timeout the way `agentux.yaml` writes it.
pub fn format_timeout(timeout: Duration) -> String {
    let secs = timeout.as_secs();
    if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_parse_with_units() {
        assert_eq!(parse_timeout("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_timeout("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["30", "m", "1.5h", "-1m", "30 min", "", "1d"] {
            assert!(parse_timeout(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_timeout("0s"), Err("must be longer than zero".into()));
        assert_eq!(format_timeout(Duration::from_secs(1800)), "30m");
        assert_eq!(format_timeout(Duration::from_secs(7200)), "2h");
        assert_eq!(format_timeout(Duration::from_secs(90)), "90s");
    }

    #[test]
    fn devcontainer_json_with_comments_and_trailing_commas() {
        let text = r#"{
            // The toolchain image.
            "name": "a // not a comment, /* nor this */",
            "image": "mcr.microsoft.com/devcontainers/rust:1", /* block
            comment */
            "features": { "x": {}, },
            "forwardPorts": [3000, 8080,],
        }"#;
        assert_eq!(
            devcontainer_image(text),
            Ok(Some("mcr.microsoft.com/devcontainers/rust:1".into()))
        );
        assert_eq!(
            devcontainer_image(r#"{ "build": { "dockerfile": "Containerfile" } }"#),
            Ok(None)
        );
        assert!(devcontainer_image(r#"{ "image": 3 }"#).is_err());
        assert!(devcontainer_image("{ image: x }").is_err());
        assert_eq!(
            devcontainer_image(r#"{ "image": "a\"b,]" }"#),
            Ok(Some("a\"b,]".into()))
        );
    }

    #[test]
    fn image_precedence() {
        let files = |devcontainer: Option<&'static str>| {
            move |path: &str| {
                Ok(devcontainer
                    .filter(|_| path == DEVCONTAINER_FILES[0])
                    .map(str::to_string))
            }
        };
        let podman = Isolation {
            mode: IsolationMode::Podman,
            ..Default::default()
        };
        let explicit = Isolation {
            image: Some("docker.io/library/rust:1".into()),
            ..podman.clone()
        };
        let image = explicit.image(files(Some(r#"{"image":"x"}"#))).unwrap();
        assert_eq!(image.reference, "docker.io/library/rust:1");
        assert_eq!(image.source, ImageSource::Config);

        let image = podman.image(files(Some(r#"{"image":"x"}"#))).unwrap();
        assert_eq!(image.reference, "x");
        assert_eq!(
            image.source,
            ImageSource::Devcontainer(".devcontainer/devcontainer.json")
        );

        let image = podman.image(files(Some(r#"{"build":{}}"#))).unwrap();
        assert_eq!(image.reference, DEFAULT_IMAGE);
        assert_eq!(
            image.source,
            ImageSource::Default(Some(".devcontainer/devcontainer.json"))
        );

        let image = podman.image(files(None)).unwrap();
        assert_eq!(image.reference, DEFAULT_IMAGE);
        assert_eq!(image.source, ImageSource::Default(None));

        let err = podman.image(files(Some(r#"{"image":"--privileged"}"#)));
        assert!(err.unwrap_err().contains("must not start with `-`"));
        let err = podman.image(files(Some("{")));
        assert!(
            err.unwrap_err()
                .starts_with(".devcontainer/devcontainer.json: not valid JSON")
        );
    }
}
