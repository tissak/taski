//! Property test: the line-based parser must **never panic** on arbitrary input
//! drawn from a small Markdown-ish alphabet.
//!
//! This is the stable-toolchain substitute for `cargo-fuzz` (which needs nightly).
//! The alphabet covers the only characters the parser branches on — bullet chars
//! (`- * +`), checkbox syntax (`[ ] x /`), whitespace, newlines, fences (`` ` ``)
//! and the newly-tolerated blockquote marker (`>`). Tier 1 extends it with the
//! characters the tag/priority/date extractors branch on (`a-zA-Z`, digits, `#`,
//! `_`, `-`); raw emoji bytes are deliberately excluded (multi-byte UTF-8 bloats
//! proptest; the emoji paths are unit-tested).

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn parse_tasks_never_panics(ref input in "[-*+\\[\\]x/ \\n`>0-9a-zA-Z#_-]{1,80}") {
        // Calling is the test: any panic fails the property. The result is
        // discarded; we only require a `Vec` comes back.
        let _tasks = taski_core::parse_tasks(input, "prop.md");
    }
}
