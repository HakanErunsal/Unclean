//! Manages persistent preset files for desktop sessions.

use std::path::{Path, PathBuf};

use crate::Result;
use crate::presets::{
    PresetDocument, PresetFile, bundled_preset_directory, list_available_presets, load_preset,
    save_preset,
};

/// Manages user presets under one application directory.
#[derive(Clone, Debug)]
pub struct PresetCatalog {
    directory: PathBuf,
}

impl PresetCatalog {
    /// Uses the supplied directory for persistent user presets.
    #[must_use]
    pub const fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// Returns the user preset directory without creating it.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Lists user presets before bundled presets and keeps the first file for each name.
    ///
    /// # Errors
    ///
    /// Returns an error when preset directory loading fails.
    pub fn list(&self) -> Result<Vec<PresetFile>> {
        list_available_presets(Some(&self.directory))
    }

    /// Saves the document in the user preset directory and preserves an existing managed path.
    ///
    /// # Errors
    ///
    /// Returns an error when directory creation or file writing fails.
    pub fn save(&self, current_path: Option<&Path>, document: &PresetDocument) -> Result<PathBuf> {
        let path = current_path
            .filter(|path| path_is_direct_child(path, &self.directory))
            .map_or_else(
                || {
                    let name = current_path
                        .and_then(Path::file_stem)
                        .and_then(|stem| stem.to_str())
                        .unwrap_or(&document.preset().name);
                    next_available_preset_path(&self.directory, name)
                },
                Path::to_path_buf,
            );
        save_preset(&path, document)
    }

    /// Loads a TOML preset and copies external files into the user preset directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the source is invalid or managed copy writing fails.
    pub fn import(&self, source: &Path) -> Result<(PathBuf, PresetDocument)> {
        let selector = source.to_string_lossy();
        let (source_path, document) = load_preset(&selector, None)?;
        if self.contains(&source_path) {
            return Ok((source_path, document));
        }

        let name = source_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(&document.preset().name);
        let managed_path = save_imported_preset(&self.directory, name, &document)?;
        Ok((managed_path, document))
    }

    /// Writes a TOML copy to the selected path and skips the managed source path.
    ///
    /// # Errors
    ///
    /// Returns an error when selected file writing fails.
    pub fn export(
        &self,
        managed_path: &Path,
        export_path: &Path,
        document: &PresetDocument,
    ) -> Result<bool> {
        if paths_match(managed_path, export_path) {
            return Ok(false);
        }
        save_preset(export_path, document)?;
        Ok(true)
    }

    fn contains(&self, path: &Path) -> bool {
        path_is_direct_child(path, &self.directory)
            || bundled_preset_directory()
                .as_deref()
                .is_some_and(|directory| path_is_direct_child(path, directory))
    }
}

/// Replaces path punctuation with hyphens for a preset TOML file name.
#[must_use]
pub fn preset_filename_stem(name: &str) -> String {
    let stem = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "preset".to_owned()
    } else {
        stem.to_owned()
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .canonicalize()
            .ok()
            .zip(right.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn path_is_direct_child(path: &Path, directory: &Path) -> bool {
    path.parent()
        .is_some_and(|parent| paths_match(parent, directory))
}

fn next_available_preset_path(directory: &Path, name: &str) -> PathBuf {
    let stem = preset_filename_stem(name);
    let first = directory.join(format!("{stem}.toml"));
    if !first.exists() {
        return first;
    }

    let mut suffix = 2;
    loop {
        let candidate = directory.join(format!("{stem}-{suffix}.toml"));
        if !candidate.exists() {
            return candidate;
        }
        suffix += 1;
    }
}

fn save_imported_preset(
    directory: &Path,
    name: &str,
    document: &PresetDocument,
) -> Result<PathBuf> {
    let first = directory.join(format!("{}.toml", preset_filename_stem(name)));
    if first.exists() {
        let selector = first.to_string_lossy();
        if let Ok((path, existing)) = load_preset(&selector, None)
            && existing.render() == document.render()
        {
            return Ok(path);
        }
    }

    save_preset(&next_available_preset_path(directory, name), document)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{PresetCatalog, paths_match, preset_filename_stem};
    use crate::presets::{PresetDocument, save_preset};

    #[test]
    fn filename_stem_replaces_path_punctuation() {
        assert_eq!(preset_filename_stem("../Lean Setup"), "Lean-Setup");
    }

    #[test]
    fn saved_presets_remain_in_the_catalog() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let directory = temp.path().join("presets");
        let catalog = PresetCatalog::new(directory.clone());
        let document = PresetDocument::new("Persistent preset")?;

        let path = catalog.save(None, &document)?;

        assert!(
            path.parent()
                .is_some_and(|parent| paths_match(parent, &directory))
        );
        assert!(
            catalog
                .list()?
                .iter()
                .any(|preset| preset.name == "Persistent-preset")
        );
        Ok(())
    }

    #[test]
    fn imports_persist_without_duplicate_copies() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let directory = temp.path().join("managed");
        let source = temp.path().join("external").join("team-preset.toml");
        let document = PresetDocument::new("Team preset")?;
        let source = save_preset(&source, &document)?;
        let catalog = PresetCatalog::new(directory.clone());

        let (first_path, _) = catalog.import(&source)?;
        let (second_path, _) = catalog.import(&source)?;

        assert_eq!(first_path, second_path);
        let restarted = PresetCatalog::new(directory.clone());
        let managed = restarted
            .list()?
            .into_iter()
            .filter(|preset| {
                preset
                    .path
                    .parent()
                    .is_some_and(|parent| paths_match(parent, &directory))
            })
            .collect::<Vec<_>>();
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0].name, "team-preset");
        Ok(())
    }
}
