use std::process::Command;

/// Run a git command and return its trimmed stdout, or `None` when git is
/// missing, the command fails, or the output is empty.
///
/// A build that happens outside a git checkout (an exported source tarball, a
/// container that copies only `src/`, a vendored crate) is a NORMAL build, not a
/// broken one: it simply has no commit to name. Returning `None` lets the
/// version string omit the git parenthetical entirely instead of printing the
/// literal word "unknown", which reads as a defect to an operator.
fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn main() {
    let git_hash = git_output(&["rev-parse", "--short", "HEAD"]);

    // A detached HEAD (every tag build, and `actions/checkout` by default)
    // reports the literal branch name "HEAD", which names nothing. Drop it and
    // let the hash stand alone rather than printing `HEAD@abc1234`.
    let git_branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|branch| branch != "HEAD")
        .filter(|_| git_hash.is_some());

    // Only ask about the working tree when we KNOW we are in a git checkout.
    // `git diff --quiet` exits 1 for "dirty" and 0 for "clean", but it also
    // exits 128/129 when there is no repository at all; treating that as dirty
    // is what produced the `-dirty` half of the `(unknown@unknown-dirty)` string
    // shipped on device. Only an exact exit code of 1 means dirty.
    let git_dirty = git_hash.is_some()
        && Command::new("git")
            .args(["diff", "--quiet"])
            .status()
            .ok()
            .and_then(|status| status.code())
            .is_some_and(|code| code == 1);

    println!(
        "cargo:rustc-env=GIT_HASH={}",
        git_hash.as_deref().unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=GIT_BRANCH={}",
        git_branch.as_deref().unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=GIT_DIRTY={}",
        if git_dirty { "-dirty" } else { "" }
    );

    // Tell Cargo to re-run this build script if git files change
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}
