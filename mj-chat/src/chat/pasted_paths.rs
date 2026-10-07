//! Image files named by pasted or dropped text.
//!
//! A terminal delivers a dropped file, and Explorer's "Copy as path", as text.
//! A container or remote session cannot open a path on this machine, so a
//! paste made only of image paths becomes image attachments instead. The
//! files are read later, on the attachment task.

use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
#[cfg(not(target_os = "linux"))]
use anyhow::bail;

const IMAGE_EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

/// A path as it was pasted, before it is resolved on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PastedPath {
    /// A path this platform opens as written.
    Local(PathBuf),
    /// A Windows drive or UNC path on a Unix client, which only WSL can open.
    Windows(String),
}

impl PastedPath {
    /// The file to read. Under WSL a Windows path is translated by running
    /// `wslpath -u`, so call this on the attachment task, never on the event
    /// loop.
    pub(crate) fn resolve(self) -> Result<PathBuf> {
        match self {
            Self::Local(path) => Ok(path),
            Self::Windows(path) => windows_path_on_this_host(&path),
        }
    }
}

/// `wslpath` knows custom automount roots and `\\wsl.localhost\…` paths,
/// which a fixed `/mnt/<drive>` mapping gets wrong.
#[cfg(target_os = "linux")]
fn windows_path_on_this_host(path: &str) -> Result<PathBuf> {
    use anyhow::{Context, bail, ensure};
    use mj_core::targets::CommandExecutor;

    if !crate::clipboard::running_under_wsl() {
        bail!("{path} is a Windows path, which this machine cannot open");
    }
    let command = mj_core::targets::CommandSpec::new("wslpath", ["-u", path])
        .purpose("translate a pasted Windows path");
    let output = mj_core::targets::CancellableProcessExecutor::with_timeout(
        std::time::Duration::from_secs(10),
    )
    .execute(&command)
    .context("run wslpath")?;
    if output.status != 0 {
        bail!(
            "wslpath could not translate {path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let translated = String::from_utf8(output.stdout).context("wslpath output was not UTF-8")?;
    let translated = translated.trim_end_matches(['\r', '\n']);
    ensure!(!translated.is_empty(), "wslpath printed no path for {path}");
    Ok(PathBuf::from(translated))
}

#[cfg(not(target_os = "linux"))]
fn windows_path_on_this_host(path: &str) -> Result<PathBuf> {
    bail!("{path} is a Windows path, which this machine cannot open")
}

/// One image path in pasted text. `range` covers it as pasted, quotes and
/// escapes included, so a failed read can put back exactly what was pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedImagePath {
    pub(crate) range: Range<usize>,
    pub(crate) path: PastedPath,
}

/// The image paths `pasted` consists of, or `None` when it holds anything
/// else. Every non-blank line must be one or more absolute PNG, JPEG or WebP
/// paths: quoted, backslash-escaped as macOS terminals drop them, `file://`
/// URLs, or Windows drive and UNC paths.
pub(crate) fn image_paths(pasted: &str) -> Option<Vec<PastedImagePath>> {
    let mut found = Vec::new();
    let mut line_offset = 0;
    for line in pasted.split('\n') {
        let offset = line_offset;
        line_offset += line.len() + 1;
        let paths = line_tokens(line)
            .and_then(|tokens| {
                tokens
                    .into_iter()
                    .map(|(range, text)| image_path(&text).map(|path| (range, path)))
                    .collect::<Option<Vec<_>>>()
            })
            // An unquoted path with spaces, as a Windows path is often pasted,
            // only parses as the whole line.
            .or_else(|| {
                let trimmed = line.trim();
                let start = line.len() - line.trim_start().len();
                image_path(trimmed).map(|path| vec![(start..start + trimmed.len(), path)])
            })?;
        found.extend(paths.into_iter().map(|(range, path)| PastedImagePath {
            range: range.start + offset..range.end + offset,
            path,
        }));
    }
    (!found.is_empty()).then_some(found)
}

/// The path `/attach` names: one quoted, escaped, `file://` or Windows path,
/// or else the argument exactly as typed.
pub(crate) fn path_argument(argument: &str) -> PastedPath {
    let argument = argument.trim();
    if let Some(tokens) = line_tokens(argument)
        && let [(_, text)] = tokens.as_slice()
    {
        return path_from(text).unwrap_or_else(|| PastedPath::Local(PathBuf::from(text)));
    }
    path_from(argument).unwrap_or_else(|| PastedPath::Local(PathBuf::from(argument)))
}

fn image_path(text: &str) -> Option<PastedPath> {
    let path = path_from(text)?;
    let name = match &path {
        PastedPath::Local(path) => path.as_path(),
        PastedPath::Windows(path) => Path::new(path),
    };
    name.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|image| extension.eq_ignore_ascii_case(image))
        })
        .then_some(path)
}

/// An absolute path named by one token, or `None`.
fn path_from(text: &str) -> Option<PastedPath> {
    if text
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file://"))
    {
        return url::Url::parse(text)
            .ok()?
            .to_file_path()
            .ok()
            .map(PastedPath::Local);
    }
    if is_windows_path(text) {
        return Some(if cfg!(windows) {
            PastedPath::Local(PathBuf::from(text))
        } else {
            PastedPath::Windows(text.to_owned())
        });
    }
    let path = PathBuf::from(text);
    path.is_absolute().then_some(PastedPath::Local(path))
}

/// A drive path such as `C:\…` or `C:/…`, or a UNC path such as `\\server\…`.
fn is_windows_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.starts_with(r"\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// Split one line into whitespace-separated tokens with their byte ranges.
/// A token is quoted with `"` or `'`, or bare with backslash escapes. A
/// Windows path keeps its backslashes, and so does every token on Windows.
/// `None` when a quote is left open or is followed by more text.
fn line_tokens(line: &str) -> Option<Vec<(Range<usize>, String)>> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some(&(start, first)) = chars.peek() {
        if first.is_whitespace() {
            chars.next();
            continue;
        }
        let mut text = String::new();
        let mut end = start;
        if matches!(first, '"' | '\'') {
            chars.next();
            loop {
                let (index, character) = chars.next()?;
                if character == first {
                    end = index + 1;
                    break;
                }
                text.push(character);
            }
            if chars
                .peek()
                .is_some_and(|(_, character)| !character.is_whitespace())
            {
                return None;
            }
        } else {
            let escapes = !cfg!(windows) && !is_windows_path(&line[start..]);
            while let Some(&(index, character)) = chars.peek() {
                if character.is_whitespace() {
                    break;
                }
                chars.next();
                end = index + character.len_utf8();
                if escapes
                    && character == '\\'
                    && let Some((index, escaped)) = chars.next()
                {
                    end = index + escaped.len_utf8();
                    text.push(escaped);
                    continue;
                }
                text.push(character);
            }
        }
        tokens.push((start..end, text));
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(pasted: &str) -> Option<Vec<(&str, PastedPath)>> {
        image_paths(pasted).map(|paths| {
            paths
                .into_iter()
                .map(|found| (&pasted[found.range], found.path))
                .collect()
        })
    }

    fn windows(path: &str) -> PastedPath {
        if cfg!(windows) {
            PastedPath::Local(PathBuf::from(path))
        } else {
            PastedPath::Windows(path.to_owned())
        }
    }

    // Each form a terminal or file manager pastes for a dropped or copied
    // file, and the prose that must stay text.
    #[test]
    fn pasted_image_paths_are_recognized_in_every_form_terminals_produce() {
        // Explorer's "Copy as path", quoted, with an apostrophe and accents.
        let explorer = r#""C:\Users\me\Pictures\Capture d'écran 2026-10-06 052845.png""#;
        assert_eq!(
            found(explorer),
            Some(vec![(
                explorer,
                windows(r"C:\Users\me\Pictures\Capture d'écran 2026-10-06 052845.png")
            )])
        );
        // Several files copied as paths arrive one per line.
        let two = "\"C:\\a b\\one.PNG\"\n\\\\wsl.localhost\\Debian\\tmp\\two.webp\n";
        assert_eq!(
            found(two),
            Some(vec![
                ("\"C:\\a b\\one.PNG\"", windows(r"C:\a b\one.PNG")),
                (
                    r"\\wsl.localhost\Debian\tmp\two.webp",
                    windows(r"\\wsl.localhost\Debian\tmp\two.webp")
                ),
            ])
        );
        // An unquoted Windows path with spaces is one path.
        assert_eq!(
            found(r"C:\My Pictures\shot.jpg"),
            Some(vec![(
                r"C:\My Pictures\shot.jpg",
                windows(r"C:\My Pictures\shot.jpg")
            )])
        );
        if cfg!(unix) {
            // macOS terminals escape spaces and drop several files on one line;
            // GNOME quotes with single quotes and adds a trailing space.
            let mac = r"/Users/me/Desktop/Screen\ Shot\ 1.png /tmp/b.jpeg ";
            assert_eq!(
                found(mac),
                Some(vec![
                    (
                        r"/Users/me/Desktop/Screen\ Shot\ 1.png",
                        PastedPath::Local("/Users/me/Desktop/Screen Shot 1.png".into())
                    ),
                    ("/tmp/b.jpeg", PastedPath::Local("/tmp/b.jpeg".into())),
                ])
            );
            assert_eq!(
                found("'/home/me/my shot.png' "),
                Some(vec![(
                    "'/home/me/my shot.png'",
                    PastedPath::Local("/home/me/my shot.png".into())
                )])
            );
            assert_eq!(
                found("file:///home/me/caf%C3%A9%20shot.png"),
                Some(vec![(
                    "file:///home/me/caf%C3%A9%20shot.png",
                    PastedPath::Local("/home/me/café shot.png".into())
                )])
            );
        }
        // Anything else stays text.
        for prose in [
            "",
            "\n\n",
            r#""C:\Users\me\shot.png" we have a graphical bug"#,
            "look at /tmp/shot.png",
            "/tmp/notes.txt",
            "relative/shot.png",
            r#""C:\unterminated.png"#,
        ] {
            assert_eq!(found(prose), None, "{prose:?}");
        }
    }
}
