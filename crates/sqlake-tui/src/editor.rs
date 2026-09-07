//! Handing the buffer to `$EDITOR`.
//!
//! The same mechanism as `git commit`: write a file, run the editor on it,
//! read it back. What that buys is the user's own configuration — highlighting,
//! completion, an LSP, their key bindings — with no editor crate here and no
//! multi-line editing, undo or search to write.
//!
//! Nothing in this module touches the terminal. Releasing it and taking it
//! back is [`crate::terminal::TerminalGuard::suspended`], because every mode
//! change in this crate goes through one place; this module only decides what
//! to run and what came back.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use sqlake_core::id::TabId;

/// Under which an exit is a return rather than an edit.
///
/// A GUI editor with no `--wait` forks and exits at once, and the client takes
/// the terminal back before anything has been typed. That is not detectable
/// with certainty — somebody can genuinely quit vim in a fifth of a second —
/// so it is reported rather than acted on.
const TOO_QUICK: Duration = Duration::from_millis(200);

/// What to run, and where the working files go.
///
/// Resolved once at startup: reading `$EDITOR` per keystroke would let a
/// variable changed in another shell take effect halfway through a session,
/// and the file would move with it.
#[derive(Debug, Clone)]
pub struct Editor {
    program: OsString,
    args: Vec<String>,
    scratch: PathBuf,
}

/// How an edit ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edited {
    /// The file came back different. Carries the new text.
    Changed(String),
    /// The editor was opened and the file is as it was. Not an error: closing
    /// without saving is a way of saying no.
    Unchanged,
    /// The editor exited immediately with the file untouched, which is what a
    /// GUI editor missing its `--wait` looks like.
    Returned,
    /// The editor could not be run, or the file could not be read back.
    Failed(String),
}

impl Editor {
    #[must_use]
    pub fn new(program: OsString, args: Vec<String>, scratch: PathBuf) -> Self {
        Self {
            program,
            args,
            scratch,
        }
    }

    /// The file a tab is edited through.
    ///
    /// `.sql`, so the editor's own filetype detection does the rest: that is
    /// the whole of what makes highlighting and an LSP work without this crate
    /// knowing they exist.
    #[must_use]
    pub fn path_for(&self, tab: TabId) -> PathBuf {
        self.scratch.join(format!("{}.sql", tab.get()))
    }

    /// Write `text`, run the editor on it, and read it back.
    ///
    /// The file is truncated from `text` every time rather than reused. Tab
    /// ids start again at one in a new session, so a file left by a previous
    /// one would otherwise open as the contents of a tab that never had them.
    ///
    /// Call it inside `TerminalGuard::suspended`: it blocks until the editor
    /// exits, and until then the editor owns the screen.
    pub fn edit(&self, path: &Path, text: &str) -> Edited {
        if let Err(why) = write(path, text) {
            return Edited::Failed(format!("could not write {}: {why}", path.display()));
        }

        let started = Instant::now();
        let status = Command::new(&self.program)
            .args(&self.args)
            .arg(path)
            .status();
        let elapsed = started.elapsed();

        match status {
            // A non-zero exit is not a refusal to use what was saved: `vim -c
            // cq` exits non-zero on purpose, and the file on disk is still
            // what the user left there.
            Ok(_) => {}
            Err(why) => {
                return Edited::Failed(format!("could not run {:?}: {why}", self.program));
            }
        }

        let back = match std::fs::read_to_string(path) {
            Ok(back) => back,
            Err(why) => return Edited::Failed(format!("could not read {}: {why}", path.display())),
        };

        if back != text {
            return Edited::Changed(back);
        }
        if elapsed < TOO_QUICK {
            return Edited::Returned;
        }
        Edited::Unchanged
    }

    /// The message for [`Edited::Returned`], which names the setting that
    /// fixes it.
    #[must_use]
    pub fn hurried(&self) -> String {
        format!(
            "{:?} exited straight away and the file is unchanged — a GUI editor \
             needs its wait flag in `editor_args`",
            self.program
        )
    }
}

fn write(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An "editor" that is a shell command, so the tests exercise the real
    /// spawn rather than a fake of it.
    fn editor(script: &str, dir: &Path) -> Editor {
        Editor::new(
            OsString::from("sh"),
            vec!["-c".to_owned(), format!("{script} \"$0\"")],
            dir.to_path_buf(),
        )
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temp dir")
    }

    #[test]
    fn what_the_editor_saved_comes_back() {
        let dir = tmp();
        let e = editor("printf 'select 2' >", dir.path());
        let path = e.path_for(TabId::new(1));
        assert_eq!(
            e.edit(&path, "select 1"),
            Edited::Changed("select 2".to_owned())
        );
    }

    #[test]
    fn an_editor_that_saves_nothing_is_not_a_failure() {
        // Closing without saving is how somebody says no, and treating it as
        // an error would put a dialog in front of a decision already made.
        let dir = tmp();
        // `sleep` so this is not read as a GUI editor that forked.
        let e = editor("sleep 0.3 && true", dir.path());
        let path = e.path_for(TabId::new(1));
        assert_eq!(e.edit(&path, "select 1"), Edited::Unchanged);
    }

    #[test]
    fn an_editor_that_returns_at_once_is_reported() {
        let dir = tmp();
        let e = editor("true", dir.path());
        let path = e.path_for(TabId::new(1));
        assert_eq!(e.edit(&path, "select 1"), Edited::Returned);
        assert!(e.hurried().contains("editor_args"));
    }

    #[test]
    fn an_editor_that_is_not_there_is_reported_rather_than_fatal() {
        let dir = tmp();
        let e = Editor::new(
            OsString::from("sqlake-no-such-editor"),
            Vec::new(),
            dir.path().to_path_buf(),
        );
        let path = e.path_for(TabId::new(1));
        assert!(matches!(e.edit(&path, ""), Edited::Failed(_)));
    }

    #[test]
    fn the_file_is_truncated_from_the_buffer_first() {
        // Tab ids start again at one in a new session, so a file left by a
        // previous one must not open as the contents of a tab that never had
        // them.
        let dir = tmp();
        let e = editor("sleep 0.3 && true", dir.path());
        let path = e.path_for(TabId::new(1));
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, "-- last week's query").unwrap();
        assert_eq!(e.edit(&path, "select 1"), Edited::Unchanged);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "select 1");
    }

    #[test]
    fn a_missing_scratch_directory_is_made() {
        let dir = tmp();
        let under = dir.path().join("not-created-yet");
        let e = editor("sleep 0.3 && true", &under);
        let path = e.path_for(TabId::new(3));
        assert_eq!(e.edit(&path, "select 1"), Edited::Unchanged);
        assert!(path.exists());
    }

    #[test]
    fn a_tab_has_a_sql_file_of_its_own() {
        // The extension is what makes the editor's filetype detection do the
        // rest, and the id is what keeps two tabs from sharing a buffer.
        let e = Editor::new(OsString::from("vi"), Vec::new(), PathBuf::from("/s"));
        assert_eq!(e.path_for(TabId::new(7)), PathBuf::from("/s/7.sql"));
        assert_ne!(e.path_for(TabId::new(7)), e.path_for(TabId::new(8)));
    }
}
