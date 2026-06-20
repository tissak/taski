//! taski-config — TOML configuration loading and CLI/config/default precedence.
//!
//! Config is optional. The default location is `~/.config/taski/config.toml`
//! (XDG-style), overridable via the `TASKI_CONFIG` environment variable (which may
//! point at any file). A missing file is not an error (yields an empty [`Config`]);
//! only a present-but-malformed file errors.
//!
//! Resolution precedence (highest wins): CLI flag → config file → compiled default.
//! `vault` has no default and is required by the daemon; `db` defaults to
//! `./taski.db`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// User configuration. Both fields are optional; a missing field falls back to the
/// CLI flag (if given) and then to the compiled default.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Config {
    /// Path to the Obsidian vault root.
    pub vault: Option<String>,
    /// Path to the taski SQLite index database.
    pub db: Option<String>,
    /// Directories (relative to vault root) to exclude from scanning and indexing.
    /// Each entry is a path component or nested path, e.g. `["templates"]` or
    /// `["templates", "archive/drafts"]`. Hidden directories (`.obsidian`, `.trash`,
    /// `.git`) are always excluded and don't need to be listed here.
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
}

/// Load config from the effective path: `$TASKI_CONFIG` if set and non-empty, else
/// `~/.config/taski/config.toml`. A missing file yields an empty [`Config`] (config
/// is optional); a present-but-unreadable or malformed file returns a clear error.
pub fn load() -> Result<Config> {
    load_from(&config_path())
}

/// Load config from an explicit path. A missing file is `Ok(Config::default())`
/// (config is optional); a present-but-malformed file is an `Err`. Exposed so callers
/// and tests can target a known file without going through the env-var/default path.
pub fn load_from(path: &Path) -> Result<Config> {
    let bytes = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e).with_context(|| format!("reading config {path:?}")),
    };
    toml::from_str(&bytes).with_context(|| format!("parsing config {path:?}"))
}

/// The effective config-file path: the value of `TASKI_CONFIG` if set and non-empty,
/// else `~/.config/taski/config.toml`. Exposed so the daemon's `--init-config` flag
/// can write to the same path [`load`] reads from.
pub fn config_path() -> PathBuf {
    config_path_from(std::env::var_os("TASKI_CONFIG"))
}

/// Pure variant of [`config_path`] that takes the `TASKI_CONFIG` value as an argument
/// so it can be tested without mutating the process environment (which is `unsafe`
/// in edition 2024 and racy under parallel tests). A set-but-empty value is treated
/// as unset.
fn config_path_from(taski_config: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(p) = taski_config
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    default_config_path()
}

/// `~/.config/taski/config.toml`, computed from `$HOME`.
///
/// Resolved manually rather than via `dirs::config_dir()`: on macOS that crate
/// returns `~/Library/Application Support`, whereas the project convention (see
/// `docs/setup.md`) is XDG-style `~/.config`. Returns a relative
/// `.config/taski/config.toml` if `$HOME` is unset (which will then simply fail to
/// load as missing → empty config).
fn default_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config").join("taski").join("config.toml")
}

/// Resolve the vault path with precedence: CLI flag → config → (no default). Returns
/// an error naming the config file and `--vault` if neither provides a value, since
/// the daemon requires a vault.
pub fn resolve_vault(cli: Option<&str>, cfg: &Config) -> Result<PathBuf> {
    if let Some(v) = cli {
        return Ok(PathBuf::from(v));
    }
    if let Some(v) = cfg.vault.as_deref() {
        return Ok(PathBuf::from(v));
    }
    anyhow::bail!(
        "no vault configured. Set `vault` in {} or pass `--vault <PATH>`",
        config_path().display()
    )
}

/// Resolve the DB path with precedence: CLI flag → config → compiled default
/// `./taski.db`. Never errors.
pub fn resolve_db(cli: Option<&str>, cfg: &Config) -> PathBuf {
    if let Some(d) = cli {
        return PathBuf::from(d);
    }
    if let Some(d) = cfg.db.as_deref() {
        return PathBuf::from(d);
    }
    PathBuf::from("./taski.db")
}

/// Render a ready-to-use config file body for `taski-daemon --init-config`. If `vault`
/// is given it is baked in as an active key; otherwise a commented placeholder is
/// emitted so the user knows to fill it in. `db` is always written (the caller
/// supplies the conventional default). The output is valid TOML that round-trips
/// through [`load_from`].
pub fn template(vault: Option<&str>, db: &str) -> String {
    let vault_line = match vault {
        Some(v) => format!("vault = {v:?}\n"),
        None => "# vault = \"/path/to/your/obsidian/vault\"\n".to_string(),
    };
    format!(
        "# Taski configuration. Generated by `taski-daemon --init-config`.\n\
         # Edit as needed; --vault/--db override these per-invocation.\n\
         # See docs/setup.md.\n\
         \n\
         {vault_line}\
         db = {db:?}\n\
         \n\
         # Directories to exclude from scanning (relative to vault root).\n\
         # Hidden directories (.obsidian, .trash, .git) are always excluded.\n\
         # exclude_dirs = [\"templates\", \"archive/drafts\"]\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `contents` to a temp file and return the held handle (so it lives for
    /// the test). Panics on I/O error.
    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    #[test]
    fn load_from_parses_both_fields() {
        let f = write_temp("vault = \"/tmp/vault\"\ndb = \"/tmp/taski.db\"\n");
        let cfg = load_from(f.path()).expect("parse");
        assert_eq!(cfg.vault.as_deref(), Some("/tmp/vault"));
        assert_eq!(cfg.db.as_deref(), Some("/tmp/taski.db"));
    }

    #[test]
    fn load_from_partial_config_only_vault() {
        let f = write_temp("vault = \"/tmp/v\"\n");
        let cfg = load_from(f.path()).expect("parse");
        assert_eq!(cfg.vault.as_deref(), Some("/tmp/v"));
        assert!(cfg.db.is_none());
    }

    #[test]
    fn load_from_missing_file_is_empty_config() {
        let cfg = load_from(Path::new("/nonexistent/taski-config-does-not-exist.toml"))
            .expect("missing file is not an error");
        assert_eq!(cfg, Config::default());
        assert!(cfg.vault.is_none());
        assert!(cfg.db.is_none());
    }

    #[test]
    fn load_from_empty_file_is_empty_config() {
        let f = write_temp("");
        let cfg = load_from(f.path()).expect("empty file");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn load_from_malformed_errors() {
        let f = write_temp("vault = \"/unterminated\n");
        let err = load_from(f.path()).expect_err("malformed should error");
        let msg = format!("{err:#}");
        assert!(msg.contains("parsing config"), "got: {msg}");
    }

    #[test]
    fn resolve_vault_cli_beats_config() {
        let cfg = Config {
            vault: Some("/from/config".into()),
            db: None,
            exclude_dirs: vec![],
        };
        let v = resolve_vault(Some("/from/cli"), &cfg).expect("ok");
        assert_eq!(v, PathBuf::from("/from/cli"));
    }

    #[test]
    fn resolve_vault_config_when_no_cli() {
        let cfg = Config {
            vault: Some("/from/config".into()),
            db: None,
            exclude_dirs: vec![],
        };
        let v = resolve_vault(None, &cfg).expect("ok");
        assert_eq!(v, PathBuf::from("/from/config"));
    }

    #[test]
    fn resolve_vault_errors_when_neither() {
        let err = resolve_vault(None, &Config::default()).expect_err("should error");
        let msg = format!("{err:#}");
        assert!(msg.contains("no vault configured"), "got: {msg}");
        // The error should steer the user toward both config and the CLI flag.
        assert!(msg.contains("--vault"), "got: {msg}");
    }

    #[test]
    fn resolve_db_cli_beats_config_and_default() {
        let cfg = Config {
            vault: None,
            db: Some("/from/config.db".into()),
            exclude_dirs: vec![],
        };
        assert_eq!(
            resolve_db(Some("/from/cli.db"), &cfg),
            PathBuf::from("/from/cli.db")
        );
    }

    #[test]
    fn resolve_db_config_when_no_cli() {
        let cfg = Config {
            vault: None,
            db: Some("/from/config.db".into()),
            exclude_dirs: vec![],
        };
        assert_eq!(resolve_db(None, &cfg), PathBuf::from("/from/config.db"));
    }

    #[test]
    fn resolve_db_default_when_neither() {
        assert_eq!(
            resolve_db(None, &Config::default()),
            PathBuf::from("./taski.db")
        );
    }

    /// `config_path_from` honors a set `TASKI_CONFIG`, ignores an empty value, and
    /// falls back to the default `~/.config/taski/config.toml` when unset. Tested via
    /// the pure variant to avoid mutating the process environment (unsafe + racy in
    /// edition 2024).
    #[test]
    fn config_path_uses_taski_config_when_set() {
        use std::ffi::OsString;
        assert_eq!(
            config_path_from(Some(OsString::from("/custom/path/to/cfg.toml"))),
            PathBuf::from("/custom/path/to/cfg.toml")
        );
    }

    #[test]
    fn config_path_ignores_empty_taski_config() {
        use std::ffi::OsString;
        let p = config_path_from(Some(OsString::new()));
        assert!(
            p.ends_with(".config/taski/config.toml"),
            "empty TASKI_CONFIG should fall back to default: {}",
            p.display()
        );
    }

    #[test]
    fn config_path_falls_back_to_default_when_unset() {
        let p = config_path_from(None);
        assert!(
            p.ends_with(".config/taski/config.toml"),
            "default path wrong: {}",
            p.display()
        );
    }

    #[test]
    fn template_with_vault_round_trips() {
        let body = template(Some("/tmp/myvault"), "/tmp/taski.db");
        assert!(body.contains("vault = \"/tmp/myvault\""));
        // Write to a temp file and parse back — must be valid TOML.
        let f = write_temp(&body);
        let cfg = load_from(f.path()).expect("template should parse");
        assert_eq!(cfg.vault.as_deref(), Some("/tmp/myvault"));
        assert_eq!(cfg.db.as_deref(), Some("/tmp/taski.db"));
    }

    #[test]
    fn template_without_vault_is_a_placeholder() {
        let body = template(None, "/tmp/taski.db");
        // No active vault key (commented out), but db is present and valid.
        assert!(body.contains("# vault ="));
        let f = write_temp(&body);
        let cfg = load_from(f.path()).expect("template should parse");
        assert!(cfg.vault.is_none());
        assert_eq!(cfg.db.as_deref(), Some("/tmp/taski.db"));
    }
}
