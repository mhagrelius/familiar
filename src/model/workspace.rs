//! A rooted piece of the filesystem the assistant may work in.
//!
//! The whole of this module's safety rests on one predicate: [`Workspace::resolve`],
//! which turns whatever the model asked for into a path *inside the root* or
//! into a refusal. Everything else calls it first.
//!
//! It is enforced here rather than at the gate, because the gate is static per
//! tool name — approving "write a file" once cannot mean "write *that* file",
//! and a path that escapes must be refused whether or not anyone approved
//! anything. The approval dialog is the second line, not the first.
//!
//! Symlinks are resolved before the check. A link inside the workspace pointing
//! at `/etc` is otherwise a hole straight through it, and that is the one form
//! of escape a string comparison cannot see.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// What the model is told when it reaches outside. Deliberately plain: it
/// should try somewhere else rather than try again cleverly.
pub const OUTSIDE: &str =
    "That path is outside the workspace. You can only read and write files under it.";

/// How much of a file to hand back. A model that asks for a 40 MB log should
/// get the start of it and a note, not the whole thing.
pub const MAX_READ: usize = 60_000;

/// How many results a search returns.
pub const MAX_MATCHES: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path resolved outside the root.
    Outside,
    /// It does not exist, or cannot be read.
    Missing(String),
    /// It exists but is not the kind of thing asked for.
    NotAFile(String),
    /// Bytes that are not text.
    Binary(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outside => f.write_str(OUTSIDE),
            Self::Missing(what) => write!(f, "{what} is not there"),
            Self::NotAFile(what) => write!(f, "{what} is not a file"),
            Self::Binary(what) => write!(f, "{what} is not text"),
        }
    }
}

/// The root, and everything that happens inside it.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Takes the root as given. It is canonicalised on every resolve rather
    /// than once here, because a root that is created or replaced while the app
    /// runs should still work.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The one predicate. A relative path is taken from the root; an absolute
    /// one must already be inside it; and `..` cannot climb out however it is
    /// spelled.
    ///
    /// The path need not exist — writing a new file has to resolve too — so the
    /// *parent* is canonicalised and the name appended.
    pub fn resolve(&self, asked: &str) -> Result<PathBuf, Refusal> {
        let asked = asked.trim();
        if asked.is_empty() {
            return Err(Refusal::Missing("that path".into()));
        }

        let requested = Path::new(asked);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };

        let root = self.root.canonicalize().map_err(|_| Refusal::Outside)?;

        // Canonicalise as much as exists, then re-append the rest. A path that
        // does not exist yet cannot be canonicalised, and refusing to write new
        // files would make the tool useless.
        let (existing, rest) = deepest_existing(&joined);
        let Ok(existing) = existing.canonicalize() else {
            return Err(Refusal::Outside);
        };
        // Only when there is something left: joining an empty path appends a
        // trailing separator, and `metadata("file/")` fails on a real file.
        let resolved = if rest.as_os_str().is_empty() {
            existing
        } else {
            existing.join(rest)
        };

        // `starts_with` on components, not on strings: "/home/matt" must not
        // match "/home/matthew".
        if !resolved.starts_with(&root) {
            return Err(Refusal::Outside);
        }
        // And nothing left over may climb.
        if resolved
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(Refusal::Outside);
        }
        Ok(resolved)
    }

    /// What is in a directory.
    pub fn list(&self, asked: &str) -> Result<String, Refusal> {
        let path = self.resolve(asked)?;
        let entries = fs::read_dir(&path).map_err(|_| Refusal::Missing(asked.to_string()))?;

        let mut listed: Vec<String> = entries
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                match entry.metadata() {
                    Ok(data) if data.is_dir() => format!("{name}/"),
                    Ok(data) => format!("{name} ({} bytes)", data.len()),
                    Err(_) => name,
                }
            })
            .collect();
        listed.sort();

        if listed.is_empty() {
            return Ok(format!("{asked} is empty."));
        }
        Ok(format!("{asked}:\n{}", listed.join("\n")))
    }

    /// A file's text.
    pub fn read(&self, asked: &str) -> Result<String, Refusal> {
        let path = self.resolve(asked)?;
        let data = fs::metadata(&path).map_err(|_| Refusal::Missing(asked.to_string()))?;
        if !data.is_file() {
            return Err(Refusal::NotAFile(asked.to_string()));
        }

        let bytes = fs::read(&path).map_err(|_| Refusal::Missing(asked.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|_| Refusal::Binary(asked.to_string()))?;
        if text.len() > MAX_READ {
            let kept: String = text.chars().take(MAX_READ).collect();
            return Ok(format!(
                "{kept}\n\n[{asked} continues past {MAX_READ} characters and was cut off here]"
            ));
        }
        Ok(text)
    }

    /// Which files contain `needle`, and the lines they contain it on.
    pub fn search(&self, needle: &str, asked: Option<&str>) -> Result<String, Refusal> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Err(Refusal::Missing("something to search for".into()));
        }
        let root = self.resolve(asked.unwrap_or("."))?;
        let lowered = needle.to_lowercase();

        let mut matches = Vec::new();
        walk(&root, &mut |path| {
            if matches.len() >= MAX_MATCHES {
                return;
            }
            let Ok(text) = fs::read_to_string(path) else {
                return;
            };
            for (number, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&lowered) {
                    let shown = line.trim();
                    let shown: String = shown.chars().take(160).collect();
                    matches.push(format!("{}:{}: {shown}", self.relative(path), number + 1));
                    if matches.len() >= MAX_MATCHES {
                        return;
                    }
                }
            }
        });

        if matches.is_empty() {
            return Ok(format!(
                "Nothing under {} contains {needle:?}.",
                asked.unwrap_or(".")
            ));
        }
        Ok(format!(
            "{} line(s) contain {needle:?}:\n{}",
            matches.len(),
            matches.join("\n")
        ))
    }

    /// Write a file, creating the directories above it.
    pub fn write(&self, asked: &str, contents: &str) -> Result<String, Refusal> {
        let path = self.resolve(asked)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| Refusal::Missing(asked.to_string()))?;
        }
        let existed = path.exists();
        fs::write(&path, contents).map_err(|_| Refusal::Missing(asked.to_string()))?;
        Ok(format!(
            "{} {asked} ({} bytes).",
            if existed { "Replaced" } else { "Wrote" },
            contents.len()
        ))
    }

    /// Write bytes, creating the directories above them.
    ///
    /// The same path check as [`Workspace::write`], because a `.docx` is not
    /// text and going around `resolve` to write one would be the hole this
    /// module exists to close. `what` names the kind of file for the sentence
    /// handed back — the model should see "Wrote a Word document", not a byte
    /// count it cannot interpret.
    pub fn write_bytes(&self, asked: &str, bytes: &[u8], what: &str) -> Result<String, Refusal> {
        let path = self.resolve(asked)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| Refusal::Missing(asked.to_string()))?;
        }
        let existed = path.exists();
        fs::write(&path, bytes).map_err(|_| Refusal::Missing(asked.to_string()))?;
        Ok(format!(
            "{} {what} to {asked} ({}).",
            if existed { "Replaced" } else { "Wrote" },
            size(bytes.len())
        ))
    }

    /// A file's bytes, for the tools that read something that is not text.
    pub fn read_bytes(&self, asked: &str) -> Result<Vec<u8>, Refusal> {
        let path = self.resolve(asked)?;
        let data = fs::metadata(&path).map_err(|_| Refusal::Missing(asked.to_string()))?;
        if !data.is_file() {
            return Err(Refusal::NotAFile(asked.to_string()));
        }
        fs::read(&path).map_err(|_| Refusal::Missing(asked.to_string()))
    }

    /// Move or rename, within the workspace at both ends.
    pub fn move_to(&self, from: &str, to: &str) -> Result<String, Refusal> {
        let source = self.resolve(from)?;
        let target = self.resolve(to)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|_| Refusal::Missing(to.to_string()))?;
        }
        fs::rename(&source, &target).map_err(|_| Refusal::Missing(from.to_string()))?;
        Ok(format!("Moved {from} to {to}."))
    }

    /// A path as the model asked for it, for reporting.
    fn relative(&self, path: &Path) -> String {
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        path.strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }
}

/// A byte count a person would read, for a file nobody can open in the chat.
fn size(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    format!("{:.0} KB", bytes as f64 / 1024.0)
}

/// Split a path into the deepest part that exists and the rest.
fn deepest_existing(path: &Path) -> (PathBuf, PathBuf) {
    let mut existing = path.to_path_buf();
    let mut rest = PathBuf::new();
    while !existing.exists() {
        let Some(parent) = existing.parent().map(Path::to_path_buf) else {
            break;
        };
        let Some(name) = existing.file_name() else {
            break;
        };
        // Not `join` on an empty path, which would leave a trailing separator
        // and turn "today.md" into "today.md/".
        rest = if rest.as_os_str().is_empty() {
            PathBuf::from(name)
        } else {
            Path::new(name).join(&rest)
        };
        if parent.as_os_str().is_empty() {
            existing = PathBuf::from(".");
            break;
        }
        existing = parent;
    }
    (existing, rest)
}

/// Every file under a directory, skipping the places nobody means to search.
fn walk(root: &Path, visit: &mut impl FnMut(&Path)) {
    const SKIP: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__"];

    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || SKIP.contains(&name.as_str()) {
            continue;
        }
        match entry.metadata() {
            Ok(data) if data.is_dir() => walk(&path, visit),
            Ok(data) if data.is_file() && data.len() < 2_000_000 => visit(&path),
            _ => {}
        }
    }
}

/// Move a path to the trash, so a deletion the model asked for is reversible.
pub fn trash(path: &Path) -> io::Result<()> {
    // Through gio, which is the desktop's trash rather than a folder of our
    // own: what it deletes turns up in Files, where a person would look.
    gio::prelude::FileExt::trash(&gio::File::for_path(path), gio::Cancellable::NONE)
        .map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(directory.path().join("src")).expect("dir");
        fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        fs::write(
            directory.path().join("README.md"),
            "# A project\n\nNotes here.\n",
        )
        .expect("write");
        let workspace = Workspace::new(directory.path());
        (directory, workspace)
    }

    #[test]
    fn a_relative_path_resolves_inside_the_root() {
        let (directory, workspace) = workspace();
        let resolved = workspace.resolve("src/main.rs").expect("inside");
        assert!(resolved.starts_with(directory.path().canonicalize().expect("root")));
    }

    #[test]
    fn dot_dot_cannot_climb_out_however_it_is_spelled() {
        let (_directory, workspace) = workspace();
        for asked in [
            "../etc/passwd",
            "src/../../etc/passwd",
            "./src/../../../etc/passwd",
            "src/../..",
        ] {
            assert_eq!(workspace.resolve(asked), Err(Refusal::Outside), "{asked}");
        }
    }

    #[test]
    fn an_absolute_path_outside_the_root_is_refused() {
        let (_directory, workspace) = workspace();
        assert_eq!(workspace.resolve("/etc/passwd"), Err(Refusal::Outside));
        assert_eq!(workspace.resolve("/"), Err(Refusal::Outside));
    }

    #[test]
    fn a_symlink_pointing_out_is_refused() {
        // The one escape a string comparison cannot see.
        let (directory, workspace) = workspace();
        let link = directory.path().join("escape");
        if std::os::unix::fs::symlink("/etc", &link).is_err() {
            return;
        }
        assert_eq!(workspace.resolve("escape/passwd"), Err(Refusal::Outside));
    }

    #[test]
    fn a_sibling_directory_with_a_shared_prefix_is_outside() {
        // "/tmp/work" must not admit "/tmp/work-other".
        let parent = tempfile::tempdir().expect("temp dir");
        let inside = parent.path().join("work");
        let sibling = parent.path().join("work-other");
        fs::create_dir_all(&inside).expect("dir");
        fs::create_dir_all(&sibling).expect("dir");

        let workspace = Workspace::new(&inside);
        let asked = sibling.join("secret.txt").to_string_lossy().to_string();
        assert_eq!(workspace.resolve(&asked), Err(Refusal::Outside));
    }

    #[test]
    fn a_file_that_does_not_exist_yet_still_resolves() {
        // Writing a new file has to be possible, so resolution cannot require
        // the path to exist.
        let (_directory, workspace) = workspace();
        assert!(workspace.resolve("src/new/deep/file.rs").is_ok());
    }

    #[test]
    fn listing_and_reading_do_what_they_say() {
        let (_directory, workspace) = workspace();
        let listed = workspace.list(".").expect("list");
        assert!(listed.contains("README.md"), "{listed}");
        assert!(listed.contains("src/"), "{listed}");

        let read = workspace.read("README.md").expect("read");
        assert!(read.contains("# A project"), "{read}");
    }

    #[test]
    fn reading_something_that_is_not_text_says_so() {
        let (directory, workspace) = workspace();
        fs::write(directory.path().join("blob.bin"), [0xff, 0xfe, 0x00]).expect("write");
        assert!(matches!(
            workspace.read("blob.bin"),
            Err(Refusal::Binary(_))
        ));
    }

    #[test]
    fn an_enormous_file_is_cut_off_with_a_note() {
        let (directory, workspace) = workspace();
        fs::write(directory.path().join("big.txt"), "x".repeat(MAX_READ * 2)).expect("write");
        let read = workspace.read("big.txt").expect("read");
        assert!(read.contains("cut off here"), "no note about the cut");
        assert!(read.len() < MAX_READ * 2);
    }

    #[test]
    fn search_finds_lines_and_names_them() {
        let (_directory, workspace) = workspace();
        let found = workspace.search("notes", None).expect("search");
        assert!(found.contains("README.md:3"), "{found}");

        let nothing = workspace.search("absent", None).expect("search");
        assert!(nothing.contains("Nothing under"), "{nothing}");
    }

    #[test]
    fn writing_creates_the_directories_above_it() {
        let (directory, workspace) = workspace();
        let outcome = workspace
            .write("docs/notes/today.md", "hello")
            .expect("write");
        assert!(outcome.starts_with("Wrote"), "{outcome}");
        assert_eq!(
            fs::read_to_string(directory.path().join("docs/notes/today.md")).expect("read"),
            "hello"
        );

        let again = workspace
            .write("docs/notes/today.md", "hello again")
            .expect("write");
        assert!(again.starts_with("Replaced"), "{again}");
    }

    #[test]
    fn writing_outside_the_root_is_refused_before_anything_is_written() {
        let (_directory, workspace) = workspace();
        let outside = std::env::temp_dir().join("familiar-should-not-exist.txt");
        let _ = fs::remove_file(&outside);

        let refused = workspace.write(&outside.to_string_lossy(), "no");
        assert_eq!(refused, Err(Refusal::Outside));
        assert!(!outside.exists(), "it wrote outside the workspace");
    }

    #[test]
    fn moving_stays_inside_at_both_ends() {
        let (directory, workspace) = workspace();
        workspace
            .move_to("README.md", "docs/README.md")
            .expect("move");
        assert!(directory.path().join("docs/README.md").exists());

        assert_eq!(
            workspace.move_to("src/main.rs", "/tmp/stolen.rs"),
            Err(Refusal::Outside)
        );
        assert!(directory.path().join("src/main.rs").exists());
    }

    #[test]
    fn search_skips_the_places_nobody_means_to_search() {
        let (directory, workspace) = workspace();
        fs::create_dir_all(directory.path().join("target")).expect("dir");
        fs::write(directory.path().join("target/build.rs"), "notes\n").expect("write");
        fs::create_dir_all(directory.path().join(".git")).expect("dir");
        fs::write(directory.path().join(".git/config"), "notes\n").expect("write");

        let found = workspace.search("notes", None).expect("search");
        assert!(!found.contains("target/"), "{found}");
        assert!(!found.contains(".git"), "{found}");
    }
}
