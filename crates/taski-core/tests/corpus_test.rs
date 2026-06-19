//! Golden-file corpus test for the line-based Markdown task parser.
//!
//! For each `*.md` file in `tests/corpus/`, parse it and compare against the
//! matching `*.expected` file. The expected file format is intentionally tiny and
//! hand-editable:
//!
//! ```text
//! <count>
//! <status token per task, in document order>
//! ```
//!
//! Status tokens: `open`, `done`, `inprogress`, or `other:<raw_checkbox_char>`.
//!
//! Additionally — since Slice 1 never extracts a due date — every corpus case
//! asserts that *all* parsed tasks have `due_date == None`. This is what makes the
//! `due_date.md` case (Tasks-plugin `📅` emoji in the body) meaningful: the line is
//! still a task, but no due date is parsed.

use std::fs;
use std::path::PathBuf;

use taski_core::{Status, parse_tasks};

/// Map a parsed status to its canonical corpus token.
fn status_token(status: &Status) -> String {
    match status {
        Status::Open => "open".to_string(),
        Status::Done => "done".to_string(),
        Status::InProgress => "inprogress".to_string(),
        Status::Other(ch) => format!("other:{ch}"),
    }
}

#[test]
fn corpus_parses_each_note_to_its_expected_tasks() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut md_files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("corpus dir must exist")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    md_files.sort();
    assert!(!md_files.is_empty(), "corpus dir has no .md fixtures");

    for md_path in md_files {
        let expected_path = md_path.with_extension("expected");
        let md =
            fs::read_to_string(&md_path).unwrap_or_else(|e| panic!("reading {:?}: {e}", md_path));
        let expected = fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("missing/bad expected file {:?}: {e}", expected_path));

        let note_path = format!(
            "{}.md",
            md_path.file_stem().expect("file_stem").to_string_lossy()
        );
        let tasks = parse_tasks(&md, &note_path);

        let mut expected_lines = expected.lines();
        let count: usize = expected_lines
            .next()
            .unwrap_or_else(|| panic!("{:?}: expected file is empty", md_path))
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{:?}: expected count is not an integer: {e}", md_path));
        let status_tokens: Vec<&str> = expected_lines
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();

        assert_eq!(
            tasks.len(),
            count,
            "{:?}: parsed task count mismatch",
            md_path
        );
        assert_eq!(
            tasks.len(),
            status_tokens.len(),
            "{:?}: expected file lists {} status tokens but {} tasks were found",
            md_path,
            status_tokens.len(),
            tasks.len()
        );

        for (idx, (task, want)) in tasks.iter().zip(status_tokens.iter()).enumerate() {
            let got = status_token(&task.status);
            assert_eq!(
                got, *want,
                "{:?}: status mismatch at task index {} (line {}): got {:?}, want {:?}",
                md_path, idx, task.line_number, got, want
            );
            assert_eq!(
                task.note_path, note_path,
                "{:?}: note_path not propagated at index {}",
                md_path, idx
            );
            // Slice 1 invariant: due dates are never extracted.
            assert!(
                task.due_date.is_none(),
                "{:?}: due_date should be None in Slice 1 at task index {}",
                md_path,
                idx
            );
        }
    }
}
