//! Publishes the canonical tree as a Dynacat widget file (push only).
//! See `docs/adapters/dynacat.md`.

use bookmoor_core::dynacat::{render_styled, GroupNames, Style, HEADER};
use bookmoor_core::wire::TreeResponse;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One widget file: which profile and which folder of it it shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub file: PathBuf,
    /// Profile name or id; the Dynacat default profile when `None`.
    pub profile: Option<String>,
    /// Folder titles from the root (empty = the whole tree).
    pub folder: Vec<String>,
    pub style: Style,
}

impl Output {
    /// `file=PATH[;profile=NAME][;folder=A/B/C][;title=T][;names=path|name][;depth=N]
    /// [;max-title=N][;icons=yes][;hide-arrow=yes][;same-tab=yes]`; a bare `PATH` also works.
    pub fn parse(spec: &str) -> Result<Output, String> {
        let bad = |part: &str| format!("cannot read \"{part}\" in Dynacat output \"{spec}\"");
        let mut out = Output::default();
        let mut seen = std::collections::BTreeSet::new();
        for (i, part) in spec.split(';').enumerate() {
            let part = part.trim();
            let (key, value) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None if i == 0 && !part.is_empty() => ("file", part),
                None => return Err(bad(part)),
            };
            if !seen.insert(key.to_string()) {
                return Err(format!(
                    "\"{key}\" given twice in Dynacat output \"{spec}\""
                ));
            }
            match (key, value) {
                ("file", v) if !v.is_empty() => out.file = v.into(),
                ("profile", v) if !v.is_empty() => out.profile = Some(v.into()),
                ("folder", v) => {
                    out.folder = v
                        .split('/')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                }
                ("title", v) if !v.is_empty() => out.style.title = Some(v.into()),
                ("names", "path") => out.style.names = GroupNames::Path,
                ("names", "name") => out.style.names = GroupNames::Name,
                ("depth", v) => out.style.depth = Some(v.parse().map_err(|_| bad(part))?),
                ("max-title", v) => out.style.max_title = Some(v.parse().map_err(|_| bad(part))?),
                ("icons", v) => out.style.icons = flag(v).ok_or_else(|| bad(part))?,
                ("hide-arrow", v) => out.style.hide_arrow = flag(v).ok_or_else(|| bad(part))?,
                ("same-tab", v) => out.style.same_tab = flag(v).ok_or_else(|| bad(part))?,
                _ => return Err(bad(part)),
            }
        }
        out.style
            .check()
            .map_err(|e| format!("Dynacat output \"{spec}\": {e}"))?;
        if out.file.as_os_str().is_empty() {
            return Err(format!("Dynacat output \"{spec}\" needs a file"));
        }
        Ok(out)
    }
}

/// The collecting page: one Dynacat page file with a widget for every output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSpec {
    pub file: PathBuf,
    /// Page name shown in Dynacat's navigation.
    pub name: String,
    /// 1 to 3 columns the widgets are spread over.
    pub columns: usize,
}

impl PageSpec {
    /// `file=PATH[;name=NAME][;columns=N]`; a bare `PATH` also works.
    pub fn parse(spec: &str) -> Result<PageSpec, String> {
        let bad = |part: &str| format!("cannot read \"{part}\" in Dynacat page \"{spec}\"");
        let mut p = PageSpec {
            file: PathBuf::new(),
            name: "Bookmoor".into(),
            columns: 1,
        };
        let mut seen = std::collections::BTreeSet::new();
        for (i, part) in spec.split(';').enumerate() {
            let part = part.trim();
            let (key, value) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None if i == 0 && !part.is_empty() => ("file", part),
                None => return Err(bad(part)),
            };
            if !seen.insert(key.to_string()) {
                return Err(format!("\"{key}\" given twice in Dynacat page \"{spec}\""));
            }
            match (key, value) {
                ("file", v) if !v.is_empty() => p.file = v.into(),
                ("name", v) if !v.is_empty() => p.name = v.into(),
                ("columns", v) => p.columns = v.parse().map_err(|_| bad(part))?,
                _ => return Err(bad(part)),
            }
        }
        if p.file.as_os_str().is_empty() {
            return Err(format!("Dynacat page \"{spec}\" needs a file"));
        }
        if p.name.chars().count() > Style::MAX_TITLE {
            return Err(format!(
                "Dynacat page \"{spec}\": name: 1 to {} characters",
                Style::MAX_TITLE
            ));
        }
        if !(1..=bookmoor_core::dynacat::MAX_PAGE_COLUMNS).contains(&p.columns) {
            return Err(format!(
                "Dynacat page \"{spec}\": columns: 1 to {}",
                bookmoor_core::dynacat::MAX_PAGE_COLUMNS
            ));
        }
        Ok(p)
    }
}

fn flag(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Some(true),
        "no" | "false" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// An output set up in the web UI: stored in the state file, written into the
/// directory given with `--dynacat-dir`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiOutput {
    pub id: String,
    /// File name inside the directory, e.g. `dev.yml`.
    pub file: String,
    pub profile_id: String,
    pub folder: Vec<String>,
    #[serde(default)]
    pub style: Style,
}

/// Most outputs the web UI may create.
pub const MAX_UI_OUTPUTS: usize = 50;

/// A plain `.yml` / `.yaml` file name: no path, no hidden or odd names, so
/// the UI can never reach outside the directory.
pub fn check_file_name(name: &str) -> Result<(), String> {
    let ext_ok = name.ends_with(".yml") || name.ends_with(".yaml");
    let chars_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !ext_ok || !chars_ok || name.starts_with('.') || name.len() > 64 {
        return Err("file name: letters, digits, dot, dash and underscore, ending in .yml or .yaml (at most 64 characters)".into());
    }
    Ok(())
}

/// The widget for `tree` (restricted to `folder`). A missing folder is an
/// error.
pub fn render(tree: &TreeResponse, folder: &[String], style: &Style) -> io::Result<String> {
    let names: Vec<&str> = folder.iter().map(String::as_str).collect();
    render_styled(&tree.nodes, &names, style)
        .ok_or_else(|| io::Error::other(format!("folder \"{}\" does not exist", folder.join("/"))))
}

/// Write the widget for `tree` (restricted to `folder`) to `path`, replacing
/// the file atomically. Returns `false` without touching the file when it is
/// already up to date, so Dynacat's reload does not fire for nothing. A
/// missing folder is an error and leaves the old file in place.
pub fn publish(
    path: &Path,
    tree: &TreeResponse,
    folder: &[String],
    style: &Style,
) -> io::Result<bool> {
    write_if_changed(path, &render(tree, folder, style)?)
}

/// Replace `path` with `yaml` atomically, unless it already holds exactly
/// that. Never overwrites a file that does not start with [`HEADER`].
pub fn write_if_changed(path: &Path, yaml: &str) -> io::Result<bool> {
    match fs::read_to_string(path) {
        Ok(old) if old == yaml => return Ok(false),
        // Never overwrite a file bookmoor did not write (a Dynacat config).
        Ok(old) if !old.trim().is_empty() && !old.starts_with(HEADER) => {
            return Err(io::Error::other(
                "the file exists and was not written by bookmoor; not overwriting it",
            ))
        }
        _ => {}
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let written = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(yaml.as_bytes())?;
        f.sync_all()?;
        drop(f);
        // Rename within one directory: Dynacat never sees a half-written file.
        fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
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
        assert!(publish(&file, &tree, &[], &Style::default()).unwrap());
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains("type: bookmarks"));
        assert!(!publish(&file, &tree, &[], &Style::default()).unwrap());
        assert!(!dir.join("bookmoor.yml.tmp").exists());
        let err = publish(&file, &tree, &["nope".to_string()], &Style::default()).unwrap_err();
        assert!(err.to_string().contains("nope"));
        assert!(file.exists(), "old file stays");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn foreign_files_are_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("bookmoor-dynacat-f-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("glance.yml");
        fs::write(&file, "pages: []\n").unwrap();
        let err = publish(&file, &Engine::new().tree(), &[], &Style::default()).unwrap_err();
        assert!(err.to_string().contains("not overwriting"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "pages: []\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_files_may_be_taken_over() {
        let dir = std::env::temp_dir().join(format!("bookmoor-dynacat-e-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("new.yml");
        fs::write(&file, "\n").unwrap();
        assert!(publish(&file, &Engine::new().tree(), &[], &Style::default()).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_names_are_plain() {
        assert!(check_file_name("dev.yml").is_ok());
        assert!(check_file_name("My-Work_1.yaml").is_ok());
        for bad in [
            "",
            "dev",
            "../x.yml",
            "a/b.yml",
            ".hidden.yml",
            "a b.yml",
            "x.txt",
        ] {
            assert!(check_file_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn style_options_parse() {
        let o = Output::parse(
            "file=a.yml;title=Work;names=name;depth=1;max-title=30;icons=yes;hide-arrow=on;same-tab=no",
        )
        .unwrap();
        assert_eq!(o.style.title.as_deref(), Some("Work"));
        assert_eq!(o.style.names, GroupNames::Name);
        assert_eq!((o.style.depth, o.style.max_title), (Some(1), Some(30)));
        assert!(o.style.icons && o.style.hide_arrow && !o.style.same_tab);
        assert!(Output::parse("file=a.yml;depth=x").is_err());
        assert!(Output::parse("file=a.yml;max-title=2").is_err());
        assert!(Output::parse("file=a.yml;icons=maybe").is_err());
    }

    #[test]
    fn output_specs_parse() {
        let o = Output::parse("a.yml").unwrap();
        assert_eq!(
            (o.file.to_str(), o.profile, o.folder.len()),
            (Some("a.yml"), None, 0)
        );
        let o = Output::parse("file=d.yml; profile=Work ;folder=Bar/Dev").unwrap();
        assert_eq!(o.file, PathBuf::from("d.yml"));
        assert_eq!(o.profile.as_deref(), Some("Work"));
        assert_eq!(o.folder, ["Bar", "Dev"]);
        assert!(Output::parse("profile=Work").is_err());
        assert!(Output::parse("file=a.yml;color=red").is_err());
        assert!(Output::parse("file=a.yml;file=b.yml").is_err());
        assert_eq!(
            Output::parse("folder =Dev;file=a.yml").unwrap().folder,
            ["Dev"]
        );
        assert!(Output::parse("file=a.yml;file=b.yml").is_err());
        assert_eq!(
            Output::parse("folder =Dev;file=a.yml").unwrap().folder,
            ["Dev"]
        );
    }

    #[test]
    fn page_specs_parse() {
        let p = PageSpec::parse("/d/page.yml").unwrap();
        assert_eq!((p.name.as_str(), p.columns), ("Bookmoor", 1));
        let p = PageSpec::parse("file=/d/p.yml; name=Links ;columns=2").unwrap();
        assert_eq!(
            (p.file, p.name.as_str(), p.columns),
            (PathBuf::from("/d/p.yml"), "Links", 2)
        );
        for bad in [
            "name=x",
            "file=a.yml;columns=0",
            "file=a.yml;columns=4",
            "file=a.yml;columns=x",
            "file=a.yml;color=red",
            "file=a.yml;file=b.yml",
        ] {
            assert!(PageSpec::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn page_file_is_rewritten_only_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("bookmoor-dynacat-p-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("page.yml");
        let yaml = bookmoor_core::dynacat::render_page("P", 1, &[]);
        assert!(write_if_changed(&file, &yaml).unwrap());
        assert!(!write_if_changed(&file, &yaml).unwrap());
        fs::write(&file, "pages: []\n").unwrap();
        assert!(write_if_changed(&file, &yaml).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
