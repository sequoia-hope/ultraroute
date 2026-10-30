//! The commit the router is built from, for its stats (`ULTRAROUTE_GIT`): empty when the build
//! cannot ask git (a source archive, a sandbox without it), which the stats then say.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=ULTRAROUTE_GIT={commit}");
    // Built again when HEAD moves: to another branch, or along this one.
    let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
    watched.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for name in watched {
        if let Some(path) = git(&["rev-parse", "--git-path", &name]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
