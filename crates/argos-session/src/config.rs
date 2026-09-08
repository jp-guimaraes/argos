//! A hand-rolled config file for one setting: the language preference.
//!
//! No `directories` crate and no TOML parser (the file's extension is
//! `.toml` for a human editing it by hand, but the format actually read and
//! written here is the `key = "value"` line syntax that's a strict subset of
//! TOML, not the full grammar) -- both would be new dependencies for
//! roughly 25 lines of code, on a file with exactly one key today. If a
//! second setting ever justifies a real parser, this is the place that
//! grows one; until then, adding a dependency to read one line would be the
//! wrong trade.
//!
//! Linux: `$XDG_CONFIG_HOME/argos/config.toml`, falling back to
//! `$HOME/.config/argos/config.toml` the same way every other
//! XDG-respecting Linux program does when the variable is unset. macOS:
//! `~/Library/Application Support/argos/config.toml` -- **not verified on
//! real hardware**, same caveat as `lang::macos_detect_lang`, since this
//! host is Linux.

use crate::lang::Lang;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const CONFIG_FILE_NAME: &str = "config.toml";

/// Split from [`config_dir`] so path resolution is a table a test can drive
/// with plain values, rather than mutating this process's real environment
/// -- env vars are global state, and Rust runs tests in parallel by
/// default.
fn config_dir_from(xdg_config_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let _ = xdg_config_home; // macOS has no XDG convention to honour.
        Some(PathBuf::from(home?).join("Library/Application Support/argos"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = xdg_config_home {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("argos"));
            }
        }
        Some(PathBuf::from(home?).join(".config/argos"))
    }
}

fn config_dir() -> Option<PathBuf> {
    config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Reads `lang = "<code>"` out of the config file, if one exists, is
/// readable, and names a language this build recognises. Anything short of
/// that -- no `$HOME`, no file, a value with no matching [`Lang::from_code`]
/// -- is `None`, which [`crate::lang::resolve_lang`] reads as "fall through
/// to the next thing in the precedence chain" rather than an error. A
/// config naming a language a downgraded Argos doesn't understand, or one
/// that predates this file existing at all, must not refuse to start.
pub fn load_lang_preference() -> Option<Lang> {
    load_lang_preference_from(&config_dir()?.join(CONFIG_FILE_NAME))
}

fn load_lang_preference_from(path: &Path) -> Option<Lang> {
    let contents = std::fs::read_to_string(path).ok()?;
    parse_lang_line(&contents)
}

/// Split out further so the parsing itself is testable with no filesystem
/// at all.
fn parse_lang_line(contents: &str) -> Option<Lang> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        if key.trim() != "lang" {
            continue;
        }
        let value = value.trim().trim_matches('"');
        return Lang::from_code(value);
    }
    None
}

/// Writes `lang = "<code>"` to the config file, creating its directory if
/// needed. Overwrites the whole file rather than editing it in place --
/// with exactly one key, round-tripping unknown keys through a hand-rolled
/// parser would be more code than the key itself, for a config file a user
/// is not expected to hand-edit other settings into today.
pub fn save_lang_preference(lang: Lang) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| {
        std::io::Error::other("could not determine the config directory (no $HOME)")
    })?;
    save_lang_preference_to(lang, &dir)
}

fn save_lang_preference_to(lang: Lang, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join(CONFIG_FILE_NAME),
        format!("lang = \"{}\"\n", lang.code()),
    )
}

/// Removes the config file entirely, putting the language menu's
/// "Automatic" choice back in effect. With exactly one key, "no saved
/// preference" and "no file" are the same state -- there is no other
/// setting a blank file would need to preserve. Deleting a file that
/// doesn't exist (already Automatic) is not an error.
pub fn clear_lang_preference() -> std::io::Result<()> {
    let Some(dir) = config_dir() else {
        return Ok(());
    };
    clear_lang_preference_from(&dir)
}

fn clear_lang_preference_from(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(dir.join(CONFIG_FILE_NAME)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_lang_line() {
        assert_eq!(parse_lang_line("lang = \"pt_BR\"\n"), Some(Lang::PtBr));
        assert_eq!(parse_lang_line("lang=\"en\"\n"), Some(Lang::En));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let contents = "# a comment\n\nlang = \"pt_BR\"\n";
        assert_eq!(parse_lang_line(contents), Some(Lang::PtBr));
    }

    #[test]
    fn an_unrecognized_language_code_is_none() {
        assert_eq!(parse_lang_line("lang = \"fr\"\n"), None);
    }

    #[test]
    fn a_missing_lang_key_is_none() {
        assert_eq!(parse_lang_line("other_key = \"value\"\n"), None);
        assert_eq!(parse_lang_line(""), None);
    }

    /// A key ending in "lang" but not equal to it -- the naive
    /// `.contains("lang")` this parser does not use would have matched it.
    #[test]
    fn a_similarly_named_key_does_not_match() {
        assert_eq!(parse_lang_line("some_other_lang = \"pt_BR\"\n"), None);
    }

    /// Round-trips through a real file in a temp directory -- driven by the
    /// `_from`/`_to` path parameters, not this process's real `$HOME`, so
    /// it is safe under parallel test execution.
    #[test]
    fn save_then_load_round_trips_through_a_real_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_lang_preference_to(Lang::PtBr, dir.path()).expect("save");
        assert_eq!(
            load_lang_preference_from(&dir.path().join(CONFIG_FILE_NAME)),
            Some(Lang::PtBr)
        );
    }

    #[test]
    fn clearing_a_saved_preference_removes_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        save_lang_preference_to(Lang::PtBr, dir.path()).expect("save");
        clear_lang_preference_from(dir.path()).expect("clear");
        assert_eq!(
            load_lang_preference_from(&dir.path().join(CONFIG_FILE_NAME)),
            None
        );
    }

    /// Clearing a preference that was never saved (already "Automatic")
    /// must not be an error -- the language menu's "Automatic" button calls
    /// this unconditionally, with no way to know in advance whether a file
    /// exists.
    #[test]
    fn clearing_when_nothing_was_ever_saved_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        clear_lang_preference_from(dir.path()).expect("clear of a missing file must succeed");
    }

    #[test]
    fn xdg_config_home_wins_over_home_on_linux_and_macos_ignores_it() {
        let xdg = OsString::from("/xdg/config");
        let home = OsString::from("/home/user");
        let resolved = config_dir_from(Some(xdg.clone()), Some(home.clone()));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(resolved, Some(PathBuf::from("/xdg/config/argos")));
        #[cfg(target_os = "macos")]
        assert_eq!(
            resolved,
            Some(PathBuf::from(
                "/home/user/Library/Application Support/argos"
            ))
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn falls_back_to_home_dot_config_when_xdg_config_home_is_unset_or_empty() {
        let home = Some(OsString::from("/home/user"));
        assert_eq!(
            config_dir_from(None, home.clone()),
            Some(PathBuf::from("/home/user/.config/argos"))
        );
        assert_eq!(
            config_dir_from(Some(OsString::new()), home),
            Some(PathBuf::from("/home/user/.config/argos")),
            "an empty XDG_CONFIG_HOME must be treated as unset"
        );
    }

    #[test]
    fn no_home_directory_at_all_resolves_to_nothing() {
        assert_eq!(config_dir_from(None, None), None);
    }
}
