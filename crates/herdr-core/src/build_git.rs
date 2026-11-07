//! Reading git's answers in build.rs: where this build sits against upstream
//! (Settings → Updates). Plain functions on git's text, so tests can check
//! them; build.rs runs git and includes this file.

/// Where upstream is: a remote named upstream, else origin's master (a
/// fork's mirror of upstream), else the local master. `exists` asks git.
pub fn upstream(exists: impl Fn(&str) -> bool) -> Option<&'static str> {
    ["upstream/master", "origin/master", "master"].into_iter().find(|r| exists(r))
}

/// The `version` in a Cargo.toml's `[package]`.
pub fn package_version(toml: &str) -> Option<String> {
    let mut package = false;
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            package = line == "[package]";
        } else if package && let Some(rest) = line.strip_prefix("version") {
            let value = rest.trim_start().strip_prefix('=')?.trim().trim_matches('"');
            return (!value.is_empty()).then(|| value.to_owned());
        }
    }
    None
}

/// `git rev-list --left-right --count upstream...HEAD`: (behind, ahead).
pub fn behind_ahead(text: &str) -> Option<(u32, u32)> {
    let mut n = text.split_whitespace().map(|n| n.parse().ok());
    Some((n.next()??, n.next()??))
}

/// The release tag on this commit, from `git tag --points-at HEAD`: the
/// fork's own (`geocine-…`) first.
pub fn release_tag(tags: &str, fork: Option<&str>) -> Option<String> {
    let tags: Vec<&str> = tags.lines().map(str::trim).filter(|t| !t.is_empty()).collect();
    let own = fork.and_then(|f| tags.iter().find(|t| t.strip_prefix(f).is_some_and(|rest| rest.starts_with('-'))));
    own.or(tags.first()).map(|t| t.to_string())
}

/// The fork's name: the branch, or the first part of its release tag
/// (`geocine` in `geocine-20261106-2310`) when no branch is checked out.
/// None when nothing is on top of upstream.
pub fn fork_name(branch: Option<&str>, tag: Option<&str>, ahead: u32) -> Option<String> {
    if ahead == 0 {
        return None;
    }
    let from_tag = || tag.and_then(|t| t.split_once('-')).map(|(name, _)| name);
    branch.filter(|b| !b.is_empty()).or_else(from_tag).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_version_reads_only_the_package() {
        let toml = "[workspace]\nversion = \"9.9.9\"\n\n[package]\nname = \"herdr-core\"\nversion = \"0.1.13\"\n[dependencies]\nversion = \"1\"\n";
        assert_eq!(package_version(toml).as_deref(), Some("0.1.13"));
        assert_eq!(package_version("[package]\nversion.workspace = true\n"), None);
        assert_eq!(package_version("[package]\nname = \"x\"\n"), None);
        assert_eq!(package_version(""), None);
    }

    #[test]
    fn upstream_is_a_remote_before_the_local_master() {
        assert_eq!(upstream(|_| true), Some("upstream/master"));
        assert_eq!(upstream(|r| r != "upstream/master"), Some("origin/master"));
        assert_eq!(upstream(|r| r == "master"), Some("master"));
        assert_eq!(upstream(|_| false), None);
    }

    #[test]
    fn counts_come_behind_then_ahead() {
        assert_eq!(behind_ahead("3\t10\n"), Some((3, 10)));
        assert_eq!(behind_ahead("0\t0"), Some((0, 0)));
        assert_eq!(behind_ahead("3"), None);
        assert_eq!(behind_ahead("fatal: bad revision"), None);
    }

    #[test]
    fn the_forks_own_tag_comes_first() {
        let tags = "v0.1.13\ngeocine-20261106-2310\n";
        assert_eq!(release_tag(tags, Some("geocine")).as_deref(), Some("geocine-20261106-2310"));
        assert_eq!(release_tag(tags, None).as_deref(), Some("v0.1.13"));
        assert_eq!(release_tag("geocinex-1\n", Some("geocine")).as_deref(), Some("geocinex-1"));
        assert_eq!(release_tag("\n", Some("geocine")), None);
    }

    #[test]
    fn a_fork_is_named_by_its_branch_or_its_tag() {
        assert_eq!(fork_name(Some("geocine"), None, 10).as_deref(), Some("geocine"));
        assert_eq!(fork_name(None, Some("geocine-20261106-2310"), 10).as_deref(), Some("geocine"));
        assert_eq!(fork_name(Some("geocine"), None, 0), None);
        assert_eq!(fork_name(None, Some("v0.1.13"), 2), None);
        assert_eq!(fork_name(None, None, 2), None);
    }
}
