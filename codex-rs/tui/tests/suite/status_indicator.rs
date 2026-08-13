//! Regression test: ensure that `StatusIndicatorWidget` sanitises ANSI escape
//! sequences so that no raw `\x1b` bytes are written into the backing
//! buffer.  Rendering logic is tricky to unit‑test end‑to‑end, therefore we
//! verify the *public* contract of `ansi_escape_line()` which the widget now
//! relies on.

use codex_ansi_escape::ansi_escape_line;
use std::path::Path;
use std::path::PathBuf;

#[test]
fn ansi_escape_line_strips_escape_sequences() {
    let text_in_ansi_red = "\x1b[31mRED\x1b[0m";

    // The returned line must contain three printable glyphs and **no** raw
    // escape bytes.
    let line = ansi_escape_line(text_in_ansi_red);

    let combined: String = line
        .spans
        .iter()
        .map(|span| span.content.to_string())
        .collect();

    assert_eq!(combined, "RED");
}

#[test]
fn palette_role_source_ownership_keeps_human_teal_exclusive() {
    let lib_rs =
        codex_utils_cargo_bin::find_resource!("src/lib.rs").expect("locate codex-tui source");
    let src_dir = lib_rs.parent().expect("lib.rs has a parent").to_path_buf();
    let style = std::fs::read_to_string(src_dir.join("style.rs")).expect("read style source");
    assert_eq!(style.matches("(45, 212, 191)").count(), 1);
    assert_eq!(style.matches("(96, 165, 250)").count(), 1);

    let mut files = Vec::new();
    collect_rust_files(&src_dir, &mut files).expect("collect TUI source files");
    let mut callers = Vec::new();
    for path in files {
        let contents = std::fs::read_to_string(&path).expect("read TUI source file");
        if contents.contains("human_prompt_style()") {
            callers.push(
                path.strip_prefix(&src_dir)
                    .expect("source path under TUI source")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    callers.sort();
    assert_eq!(callers, ["history_cell/messages.rs", "style.rs"]);
}

fn collect_rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}
