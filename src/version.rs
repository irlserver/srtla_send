//! `--version` output composition.
//!
//! The version line is an operator-visible surface: control UIs shell out to
//! `srtla_send -v` and render the raw stdout. So the string has to stay honest
//! when the build carried no git context. A shipped binary once read
//!
//! ```text
//! 3.2.0 (unknown@unknown-dirty) [srtla_send]
//! ```
//!
//! which claims a branch called "unknown", a commit called "unknown", and
//! uncommitted changes, none of which are true. `build.rs` emits empty strings
//! for anything it could not resolve and [`compose_version_line`] omits the
//! parenthetical entirely, matching the convention of every other CLI that
//! embeds build metadata.

/// Compose the `--version` line from the crate identity plus whatever git
/// metadata the build could resolve.
///
/// `branch`, `hash`, and `dirty` are the raw `build.rs` values: each is empty
/// when it could not be determined. The parenthetical is emitted only when
/// there is a commit to name.
///
/// | branch | hash | dirty | output |
/// |--------|------|-------|--------|
/// | `main` | `abc1234` | `` | `3.2.0 (main@abc1234) [srtla_send]` |
/// | `main` | `abc1234` | `-dirty` | `3.2.0 (main@abc1234-dirty) [srtla_send]` |
/// | `` | `abc1234` | `` | `3.2.0 (abc1234) [srtla_send]` |
/// | `` | `` | `` | `3.2.0 [srtla_send]` |
pub fn compose_version_line(
    version: &str,
    branch: &str,
    hash: &str,
    dirty: &str,
    package: &str,
) -> String {
    match build_metadata(branch, hash, dirty) {
        Some(metadata) => format!("{version} ({metadata}) [{package}]"),
        None => format!("{version} [{package}]"),
    }
}

/// The `branch@hash-dirty` build-metadata fragment, or `None` when the build had
/// no commit to name.
fn build_metadata(branch: &str, hash: &str, dirty: &str) -> Option<String> {
    // The hash is what identifies the build. A branch without one names nothing
    // reproducible, so it is never emitted alone.
    if hash.is_empty() {
        return None;
    }
    if branch.is_empty() {
        Some(format!("{hash}{dirty}"))
    } else {
        Some(format!("{branch}@{hash}{dirty}"))
    }
}

/// The `--version` line for this binary, using the metadata baked in at build
/// time by `build.rs`.
pub fn version_line() -> String {
    compose_version_line(
        env!("CARGO_PKG_VERSION"),
        env!("GIT_BRANCH"),
        env!("GIT_HASH"),
        env!("GIT_DIRTY"),
        env!("CARGO_PKG_NAME"),
    )
}

#[cfg(test)]
mod tests {
    use super::{compose_version_line, version_line};

    #[test]
    fn full_git_context_is_rendered_verbatim() {
        assert_eq!(
            compose_version_line("3.2.0", "main", "abc1234", "", "srtla_send"),
            "3.2.0 (main@abc1234) [srtla_send]"
        );
    }

    #[test]
    fn dirty_worktree_suffixes_the_hash() {
        assert_eq!(
            compose_version_line("3.2.0", "main", "abc1234", "-dirty", "srtla_send"),
            "3.2.0 (main@abc1234-dirty) [srtla_send]"
        );
    }

    #[test]
    fn detached_head_falls_back_to_the_bare_hash() {
        assert_eq!(
            compose_version_line("3.2.0", "", "abc1234", "", "srtla_send"),
            "3.2.0 (abc1234) [srtla_send]"
        );
    }

    /// The regression this module exists for: a build outside a git checkout
    /// must omit the parenthetical, never print the word "unknown".
    #[test]
    fn no_git_context_omits_the_parenthetical() {
        assert_eq!(
            compose_version_line("3.2.0", "", "", "", "srtla_send"),
            "3.2.0 [srtla_send]"
        );
    }

    /// A branch with no hash names nothing reproducible, so it is dropped too
    /// rather than rendering a half-empty `main@`.
    #[test]
    fn branch_without_a_hash_is_dropped() {
        assert_eq!(
            compose_version_line("3.2.0", "main", "", "", "srtla_send"),
            "3.2.0 [srtla_send]"
        );
    }

    /// Whatever this build resolved, the emitted line must never contain the
    /// placeholder text that shipped on device.
    #[test]
    fn built_version_line_never_says_unknown() {
        let line = version_line();
        assert!(
            !line.contains("unknown"),
            "version line leaked a placeholder: {line}"
        );
        assert!(
            !line.contains("()"),
            "version line has an empty parenthetical: {line}"
        );
        assert!(line.starts_with(env!("CARGO_PKG_VERSION")));
        assert!(line.ends_with(concat!("[", env!("CARGO_PKG_NAME"), "]")));
    }
}
