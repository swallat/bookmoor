//! Publishes the canonical tree as a Dynacat widget file (push only).
//! See `docs/adapters/dynacat.md`.

use bookmoor_core::dynacat::render_widget;
use bookmoor_core::wire::TreeResponse;
use std::fs;
use std::io;
use std::path::Path;

/// Write the widget for `tree` to `path`, replacing the file atomically.
/// Returns `false` without touching the file when it is already up to date,
/// so Dynacat's reload does not fire for nothing.
pub fn publish(path: &Path, tree: &TreeResponse) -> io::Result<bool> {
    let yaml = render_widget(&tree.nodes);
    if fs::read_to_string(path).is_ok_and(|old| old == yaml) {
        return Ok(false);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    fs::write(&tmp, yaml)?;
    // Rename within one directory: Dynacat never sees a half-written file.
    fs::rename(&tmp, path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;

    #[test]
    fn writes_once_and_skips_unchanged_content() {
        let dir = std::env::temp_dir().join(format!("bookmoor-dynacat-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bookmoor.yml");
        let tree = Engine::new().tree();
        assert!(publish(&file, &tree).unwrap());
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains("type: bookmarks"));
        assert!(!publish(&file, &tree).unwrap());
        assert!(!dir.join("bookmoor.yml.tmp").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
