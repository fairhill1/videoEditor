//! The user's own settings directory: what is kept between sessions and
//! across projects, rather than in any one project — the recent colours, the
//! saved title styles.

use std::path::PathBuf;

/// `name` in the editor's directory under the user's config.
pub fn path(name: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("videoEditor").join(name))
}

/// What `name` holds, or `None` when there is no such file or it will not
/// read.
pub fn read(name: &str) -> Option<String> {
    path(name).and_then(|p| std::fs::read_to_string(p).ok())
}

/// Replace `name` with `text`, making the directory if it is not there yet.
/// A failure is logged rather than reported: losing a setting is not worth
/// stopping the edit that changed it.
pub fn write(name: &str, text: &str) {
    let Some(path) = path(name) else {
        return;
    };
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&path, text));
    if let Err(e) = written {
        log::warn!("could not save {}: {e}", path.display());
    }
}
