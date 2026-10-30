//! Browsers yt-dlp can take cookies from, as `cookies_from_browser` values.

use std::path::Path;

struct Browser {
    /// The `cookies_from_browser` value, or for a fork, the browser it reads as.
    name: &'static str,
    label: &'static str,
    /// App bundle names, without `.app`.
    apps: &'static [&'static str],
    /// Its folder under `~/Library/Application Support`; empty for Safari.
    profile: &'static str,
    /// A fork yt-dlp reads through `name` and an absolute profile path.
    fork: bool,
}

const fn browser(
    name: &'static str,
    label: &'static str,
    apps: &'static [&'static str],
    profile: &'static str,
) -> Browser {
    Browser {
        name,
        label,
        apps,
        profile,
        fork: false,
    }
}

const fn fork(
    name: &'static str,
    label: &'static str,
    apps: &'static [&'static str],
    profile: &'static str,
) -> Browser {
    Browser {
        name,
        label,
        apps,
        profile,
        fork: true,
    }
}

// Chromium forks that key their cookies under their own Keychain name, such as
// Helium and Arc, are left out: yt-dlp asks for "Chromium Safe Storage" and
// cannot decrypt them.
#[rustfmt::skip]
const BROWSERS: &[Browser] = &[
    browser("safari", "Safari", &["Safari"], ""),
    browser("chrome", "Chrome", &["Google Chrome"], "Google/Chrome"),
    browser("firefox", "Firefox", &["Firefox", "Firefox Nightly", "Firefox Developer Edition"], "Firefox"),
    browser("brave", "Brave", &["Brave Browser"], "BraveSoftware/Brave-Browser"),
    browser("edge", "Edge", &["Microsoft Edge"], "Microsoft Edge"),
    browser("chromium", "Chromium", &["Chromium"], "Chromium"),
    browser("opera", "Opera", &["Opera"], "com.operasoftware.Opera"),
    browser("vivaldi", "Vivaldi", &["Vivaldi"], "Vivaldi"),
    browser("whale", "Whale", &["Whale", "Naver Whale"], "Naver/Whale"),
    fork("firefox", "Zen", &["Zen", "Zen Browser"], "zen"),
    fork("firefox", "LibreWolf", &["LibreWolf"], "librewolf"),
];

/// `(value, label)` for each browser installed with a profile, after "None".
/// Off macOS, every browser yt-dlp names, since nothing here knows where they live.
pub fn installed() -> Vec<(String, String)> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    match home {
        Some(home) if cfg!(target_os = "macos") => installed_in(&home, Path::new("/Applications")),
        _ => none()
            .chain(
                BROWSERS
                    .iter()
                    .filter(|b| !b.fork)
                    .map(|b| (b.name.into(), b.label.into())),
            )
            .collect(),
    }
}

fn none() -> impl Iterator<Item = (String, String)> {
    std::iter::once((String::new(), "None".into()))
}

fn installed_in(home: &Path, applications: &Path) -> Vec<(String, String)> {
    let support = home.join("Library/Application Support");
    let app = |name: &str| {
        let bundle = format!("{name}.app");
        applications.join(&bundle).exists() || home.join("Applications").join(&bundle).exists()
    };
    let found = BROWSERS.iter().filter_map(|b| {
        if !b.apps.iter().any(|a| app(a)) {
            return None;
        }
        let profile = support.join(b.profile);
        if !b.profile.is_empty() && !profile.is_dir() {
            return None;
        }
        let value = if b.fork {
            format!("{}:{}", b.name, profile.display())
        } else {
            b.name.into()
        };
        Some((value, b.label.into()))
    });
    none().chain(found).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_browsers_with_an_app_and_a_profile_are_offered() {
        let root = std::env::temp_dir().join(format!("rr-browsers-{}", uuid::Uuid::new_v4()));
        let apps = root.join("Applications");
        let support = root.join("home/Library/Application Support");
        for dir in [
            apps.join("Safari.app"),
            apps.join("Firefox.app"),
            apps.join("Zen Browser.app"),
            // An app with no profile yet, and a profile left by a removed app.
            apps.join("Google Chrome.app"),
            support.join("BraveSoftware/Brave-Browser"),
            support.join("Firefox"),
            support.join("zen"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let got = installed_in(&root.join("home"), &apps);
        let zen = format!("firefox:{}", support.join("zen").display());
        let want: Vec<(String, String)> = [
            ("", "None"),
            ("safari", "Safari"),
            ("firefox", "Firefox"),
            (zen.as_str(), "Zen"),
        ]
        .iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect();
        assert_eq!(got, want);
        std::fs::remove_dir_all(root).unwrap();
    }
}
