//! Property tests for [`taski_core::extract_tags`].
//!
//! Three properties are checked over arbitrary short strings:
//! 1. The extractor never panics (it must always return a `Vec`).
//! 2. Every returned tag matches the documented grammar: starts with an ASCII
//!    letter or `_`, continues with letters/digits/`_`/`-`/`/`. No tag may
//!    retain its `#` prefix.
//! 3. Dedup invariant: the output never contains a duplicate tag (set size
//!    equals vec length).
//!
//! The alphabet is the full `.{0,80}` strategy (`proptest`'s string regex
//! generates arbitrary UTF-8); we do not exclude any characters because the
//! extractor must be total. Raw emoji bytes do end up here, but that is fine —
//! the extractor is UTF-8-safe by construction.

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Property 1: never panics, always returns a Vec.
    #[test]
    fn extract_tags_never_panics(ref input in ".{0,80}") {
        let _tags = taski_core::extract_tags(input);
    }

    /// Property 2: every returned tag matches the grammar
    /// `^[A-Za-z_][A-Za-z0-9_/-]*$` and contains no `#`.
    #[test]
    fn extract_tags_output_matches_grammar(ref input in ".{0,80}") {
        let tags = taski_core::extract_tags(input);
        for tag in &tags {
            // No leading `#`.
            prop_assert!(
                !tag.starts_with('#'),
                "tag must not retain its '#': got {tag:?} from input {input:?}"
            );
            // First char must be ASCII letter or `_`.
            let first = tag.chars().next();
            prop_assert!(
                matches!(first, Some(c) if c.is_ascii_alphabetic() || c == '_'),
                "tag must start with ASCII letter or '_': got {tag:?} from input {input:?}"
            );
            // Rest must be letters/digits/`_`/`-`/`/`.
            for ch in tag.chars() {
                prop_assert!(
                    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '/',
                    "tag contains disallowed char {ch:?} in {tag:?} from input {input:?}"
                );
            }
        }
    }

    /// Property 3: output is deduped (no duplicates).
    #[test]
    fn extract_tags_output_is_deduped(ref input in ".{0,80}") {
        let tags = taski_core::extract_tags(input);
        let mut seen = std::collections::HashSet::new();
        for tag in &tags {
            prop_assert!(
                seen.insert(tag.clone()),
                "duplicate tag {tag:?} in {tags:?} from input {input:?}"
            );
        }
    }
}
