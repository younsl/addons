//! Startup logo, written to stderr so stdout stays pure JSON logs.

use crate::config::BuildInfo;

const LOGO: &str = r" _            _     _
| |_    ___  | |_  | |__     ___   _ __
| __|  / _ \ | __| | '_ \   / _ \ | '__|
| |_  |  __/ | |_  | | | | |  __/ | |
 \__|  \___|  \__| |_| |_|  \___| |_|";

pub fn render(build: BuildInfo) -> String {
    format!(
        "{LOGO}\n\n dotfiles, tethered home. v{} (commit {}, rustc {})\n",
        build.version, build.commit, build.rustc
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_logo_and_build() {
        let build = BuildInfo::CURRENT;
        let banner = render(build);
        assert!(banner.starts_with(LOGO));
        assert!(banner.contains(&format!("v{}", build.version)), "{banner}");
        assert!(banner.ends_with('\n'));
        assert_eq!(LOGO.lines().count(), 5);
    }
}
