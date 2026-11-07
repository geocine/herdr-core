use std::path::Path;
use std::process::{Command, Stdio};

#[path = "src/build_git.rs"]
mod build_git;

fn main() {
    build_info(Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").unwrap()));
    println!("cargo:rerun-if-changed=../../distribution/macos-Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let plist = include_str!("../../distribution/macos-Info.plist")
        .replace("@VERSION@", &std::env::var("CARGO_PKG_VERSION").unwrap());
    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("Info.plist");
    std::fs::write(&path, plist).unwrap();
    // Separate arguments preserve paths containing spaces.
    for arg in ["-sectcreate", "__TEXT", "__info_plist"] {
        println!("cargo:rustc-link-arg-bin=herdr-core=-Wl,{arg}");
    }
    println!("cargo:rustc-link-arg-bin=herdr-core={}", path.display());
}

/// Where this build sits against upstream, for `server.status` (the app's
/// Settings → Updates): the upstream commit it's based on and the fork on
/// top, as `HERDR_BUILD_*` for `option_env!`. The same names set where cargo
/// runs (a release script, CI) win over git. No git or no checkout: none of
/// them, and the status says nothing about it.
const BUILD_KEYS: [&str; 7] = [
    "HERDR_BUILD_SHA",
    "HERDR_BUILD_FORK",
    "HERDR_BUILD_AHEAD",
    "HERDR_BUILD_TAG",
    "HERDR_BUILD_UPSTREAM_VERSION",
    "HERDR_BUILD_UPSTREAM_SHA",
    "HERDR_BUILD_BEHIND",
];

fn build_info(dir: &Path) {
    let git = |args: &[&str]| -> Option<String> {
        let out =
            Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (out.status.success() && !text.is_empty()).then_some(text)
    };
    let mut found: Vec<(&str, String)> = Vec::new();
    if let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) {
        // Again when a commit, checkout, fetch or tag moves a ref; not on every build.
        // A worktree's paths are absolute, the main checkout's relative.
        let watch = ["HEAD", "packed-refs", "refs/heads", "refs/remotes", "refs/tags"];
        let args: Vec<&str> =
            std::iter::once("rev-parse").chain(watch.iter().flat_map(|w| ["--git-path", *w])).collect();
        for path in git(&args).unwrap_or_default().lines().filter(|p| dir.join(p).exists()) {
            println!("cargo:rerun-if-changed={path}");
        }
        let branch = git(&["symbolic-ref", "--short", "-q", "HEAD"]);
        let tag =
            git(&["tag", "--points-at", "HEAD"]).and_then(|tags| build_git::release_tag(&tags, branch.as_deref()));
        let upstream =
            build_git::upstream(|r| git(&["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")]).is_some());
        if let Some(upstream) = upstream
            && let Some(base) = git(&["merge-base", "HEAD", upstream])
            && let Some((behind, ahead)) = git(&["rev-list", "--left-right", "--count", &format!("{upstream}...HEAD")])
                .and_then(|t| build_git::behind_ahead(&t))
        {
            if let Some(version) =
                git(&["show", &format!("{base}:./Cargo.toml")]).and_then(|t| build_git::package_version(&t))
            {
                found.push(("HERDR_BUILD_UPSTREAM_VERSION", version));
            }
            found.push(("HERDR_BUILD_UPSTREAM_SHA", git(&["rev-parse", "--short", &base]).unwrap_or(base)));
            found.push(("HERDR_BUILD_BEHIND", behind.to_string()));
            found.push(("HERDR_BUILD_AHEAD", ahead.to_string()));
            if let Some(fork) = build_git::fork_name(branch.as_deref(), tag.as_deref(), ahead) {
                found.push(("HERDR_BUILD_FORK", fork));
            }
        }
        found.push(("HERDR_BUILD_SHA", sha));
        if let Some(tag) = tag {
            found.push(("HERDR_BUILD_TAG", tag));
        }
    }
    for key in BUILD_KEYS {
        println!("cargo:rerun-if-env-changed={key}");
        let given = std::env::var(key).ok().filter(|v| !v.is_empty());
        if let Some(value) = given.or_else(|| found.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())) {
            println!("cargo:rustc-env={key}={value}");
        }
    }
}
