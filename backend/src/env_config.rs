use std::{env, ffi::OsString, path::PathBuf};

/// Read a branded env var while still accepting the legacy key during the
/// rename window.
pub fn var(primary: &str, legacy: &str) -> Result<String, env::VarError> {
    match env::var(primary) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => env::var(legacy),
        Err(error) => Err(error),
    }
}

/// OS-string variant of `var`, used for paths where UTF-8 is not guaranteed.
pub fn var_os(primary: &str, legacy: &str) -> Option<OsString> {
    env::var_os(primary).or_else(|| env::var_os(legacy))
}

/// Prefer the new branded on-disk default, but reuse the legacy path if it
/// already exists so upgrades keep their existing database and caches.
pub fn default_path(primary: &str, legacy: &str) -> PathBuf {
    let primary_path = PathBuf::from(primary);
    if primary_path.exists() {
        return primary_path;
    }

    let legacy_path = PathBuf::from(legacy);
    if legacy_path.exists() {
        return legacy_path;
    }

    primary_path
}
