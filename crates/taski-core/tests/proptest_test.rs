//! Property test: the line-based parser must **never panic** on arbitrary input
//! drawn from a small Markdown-ish alphabet.
//!
//! This is the stable-toolchain substitute for `cargo-fuzz` (which needs nightly).
//! The alphabet covers the only characters the parser branches on — bullet chars
//! (`- * +`), checkbox syntax (`[ ] x /`), whitespace, newlines, fences (`` ` ``)
//! and the newly-tolerated blockquote marker (`>`).

use proptest::prelude::*;

proptest! {
    #[test]
    fn parse_tasks_never_panics(ref input in "[-*+\\[\\]x/ \\n`>]*") {
        // Calling is the test: any panic fails the property. The result is
        // discarded; we only require a `Vec` comes back.
        let _tasks = taski_core::parse_tasks(input, "prop.md");
    }
}
