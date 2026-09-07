//! What the design document says about the repository, checked against it.
//!
//! Outside `src` because it is not about the binary: it is about whether a
//! sentence in `docs/` is still true, which is observable only from where both
//! the document and the crates can be seen.
//!
//! One list, not every claim. A crate that exists and is not in the tree — or
//! one in the tree that was never built — is the drift that reaches a reader as
//! a plain lie about what is there, and it is the only claim in that document
//! shaped like something a machine can settle.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/sqlake`.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two above this crate")
        .to_path_buf()
}

/// The crate names drawn in the tree in §2.
///
/// The commented-out lines below it are the crates that do not exist yet, and
/// they are deliberately not matched: a `#` line is a plan, and the point of
/// this check is that the tree holds only what is real.
fn documented() -> BTreeSet<String> {
    let design = std::fs::read_to_string(repo().join("docs/design.md")).expect("design.md");
    // Scoped to §2's own fenced block: `docs/design.md` draws more than one
    // tree, and the state directory in §9 has a `sqlake.log` in it.
    let section = design
        .split_once("## 2. Workspace layout")
        .expect("§2 is where the layout is")
        .1;
    let block = section
        .split_once("```")
        .and_then(|(_, rest)| rest.split_once("```"))
        .expect("§2 opens with a fenced tree")
        .0;

    block
        .lines()
        .filter_map(|line| {
            let rest = line
                .trim()
                .strip_prefix("├── ")
                .or(line.trim().strip_prefix("└── "))?;
            let name = rest.split_whitespace().next()?.trim_end_matches('/');
            name.starts_with("sqlake").then(|| name.to_owned())
        })
        .collect()
}

fn on_disk() -> BTreeSet<String> {
    std::fs::read_dir(repo().join("crates"))
        .expect("crates/")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry
                .path()
                .join("Cargo.toml")
                .exists()
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect()
}

#[test]
fn the_workspace_layout_lists_the_crates_that_exist() {
    let (documented, on_disk) = (documented(), on_disk());
    assert!(
        !documented.is_empty(),
        "the tree in design.md §2 parsed to nothing, so this check is proving nothing"
    );

    let missing: Vec<_> = on_disk.difference(&documented).collect();
    let phantom: Vec<_> = documented.difference(&on_disk).collect();
    assert!(
        missing.is_empty() && phantom.is_empty(),
        "design.md §2 is out of date — built but not listed: {missing:?}; \
         listed but not built: {phantom:?}"
    );
}
