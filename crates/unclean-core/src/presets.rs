//! Loads, validates, and resolves portable preset intent into named plugin changes.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use globset::{GlobBuilder, GlobMatcher};
use serde::Serialize;
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, value};

use crate::platform::{install_file, replace_file};
use crate::{Error as ProductError, Result};

/// Identifies the only preset schema accepted by this build.
pub const PRESET_SCHEMA: i64 = 1;

static PRESET_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Describes one portable preset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Preset {
    /// Identifies the file schema.
    pub schema: i64,
    /// Names the preset in interface output.
    pub name: String,
    /// Describes the intended use when the author supplies one.
    pub description: Option<String>,
    /// Lists plugins whose descriptor state becomes enabled.
    pub enable: Vec<String>,
    /// Lists plugins whose descriptor state becomes disabled.
    pub disable: Vec<String>,
    /// Lists plugins that lose the `EnabledByDefault` field.
    pub clear: Vec<String>,
    /// Lists case-insensitive plugin-name patterns whose descriptor state becomes disabled.
    pub disable_matching: Vec<String>,
}

/// Identifies the descriptor action requested by one preset rule.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetAction {
    /// Requests an explicit enabled state.
    Enable,
    /// Requests an explicit disabled state.
    Disable,
    /// Requests removal of the declared state.
    Clear,
}

impl PresetAction {
    /// Returns the stable lowercase label used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Clear => "clear",
        }
    }
}

/// Identifies the editable rule list in a preset document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresetRuleList {
    /// Selects exact enable rules.
    Enable,
    /// Selects exact disable rules.
    Disable,
    /// Selects exact clear rules.
    Clear,
    /// Selects disable patterns.
    DisableMatching,
}

impl PresetRuleList {
    const fn key(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Clear => "clear",
            Self::DisableMatching => "disable_matching",
        }
    }
}

/// Identifies how one preset rule selected a plugin.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetRuleSource {
    /// Selects one case-insensitive plugin name.
    Exact,
    /// Selects a case-insensitive plugin-name pattern.
    Pattern,
}

/// Records one rule that selected a resolved plugin.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PresetRuleMatch {
    /// Identifies exact-name or pattern selection.
    pub source: PresetRuleSource,
    /// Retains the authored rule.
    pub rule: String,
}

/// Records one resolved descriptor action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PresetChange {
    /// Names the selected plugin using the scanned descriptor casing.
    pub plugin: String,
    /// Identifies the requested descriptor action.
    pub action: PresetAction,
    /// Lists every rule that selected the same action.
    pub matched_by: Vec<PresetRuleMatch>,
}

/// Shows every plugin name selected by one pattern.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PresetPatternExpansion {
    /// Retains the authored pattern.
    pub pattern: String,
    /// Lists every matching plugin using the scanned descriptor casing.
    pub matches: Vec<String>,
}

/// Records one exact rule that did not exist in the scanned plugin set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnmatchedPresetRule {
    /// Identifies the requested descriptor action.
    pub action: PresetAction,
    /// Retains the authored plugin name.
    pub rule: String,
}

/// Groups deterministic preset changes and review data for one plugin set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PresetResolution {
    /// Lists resolved changes in stable plugin-name order.
    pub changes: Vec<PresetChange>,
    /// Expands every authored pattern into its complete stable match list.
    pub pattern_expansions: Vec<PresetPatternExpansion>,
    /// Lists exact names that are absent from this plugin set.
    pub unmatched: Vec<UnmatchedPresetRule>,
}

/// Identifies one preset file without parsing its contents.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PresetFile {
    /// Uses the file stem as the selectable preset name.
    pub name: String,
    /// Records the preset file path.
    pub path: PathBuf,
}

/// Reports invalid preset syntax or schema content.
#[derive(Debug, Error)]
pub enum PresetError {
    /// Reports TOML parse failures.
    #[error("Preset TOML is invalid: {message}")]
    InvalidToml {
        /// Retains the parser diagnostic.
        message: String,
    },
    /// Reports schema content that violates preset rules.
    #[error("Preset schema is invalid: {message}")]
    InvalidSchema {
        /// States the rejected field or rule.
        message: String,
    },
}

/// Owns an editable TOML document and its validated schema 1 value.
#[derive(Clone, Debug)]
pub struct PresetDocument {
    document: DocumentMut,
    preset: Preset,
}

impl PresetDocument {
    /// Creates an empty schema 1 preset with the supplied display name.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty.
    pub fn new(name: &str) -> std::result::Result<Self, PresetError> {
        let mut document = DocumentMut::new();
        document["schema"] = value(PRESET_SCHEMA);
        document["name"] = value(name);
        document["enable"] = value(Array::new());
        document["disable"] = value(Array::new());
        document["clear"] = value(Array::new());
        document["disable_matching"] = value(Array::new());
        Self::parse(&document.to_string())
    }

    /// Parses and validates a schema 1 preset while retaining comments and formatting.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid TOML, unsupported keys, wrong field types, duplicate rules, conflicts, or invalid patterns.
    pub fn parse(source: &str) -> std::result::Result<Self, PresetError> {
        let document = source
            .parse::<DocumentMut>()
            .map_err(|error| PresetError::InvalidToml {
                message: error.to_string(),
            })?;
        let preset = parse_preset(&document)?;
        Ok(Self { document, preset })
    }

    /// Returns the validated preset value.
    #[must_use]
    pub const fn preset(&self) -> &Preset {
        &self.preset
    }

    /// Returns the current comment-preserving TOML text.
    #[must_use]
    pub fn render(&self) -> String {
        self.document.to_string()
    }

    /// Changes the displayed preset name without rewriting unrelated TOML content.
    ///
    /// # Errors
    ///
    /// Returns an error when the new name is empty or the resulting document fails validation.
    pub fn set_name(&mut self, name: &str) -> std::result::Result<(), PresetError> {
        let mut document = self.document.clone();
        document["name"] = value(name);
        self.replace_document(&document)
    }

    /// Changes or removes the optional description without rewriting unrelated TOML content.
    ///
    /// # Errors
    ///
    /// Returns an error when the resulting document fails validation.
    pub fn set_description(
        &mut self,
        description: Option<&str>,
    ) -> std::result::Result<(), PresetError> {
        let mut document = self.document.clone();
        if let Some(description) = description {
            document["description"] = value(description);
        } else {
            document.remove("description");
        }
        self.replace_document(&document)
    }

    /// Changes one rule list without rewriting unrelated TOML content.
    ///
    /// # Errors
    ///
    /// Returns an error when a rule is empty, duplicated, conflicting, or an invalid pattern.
    pub fn set_rules(
        &mut self,
        list: PresetRuleList,
        rules: &[String],
    ) -> std::result::Result<(), PresetError> {
        let mut array = Array::new();
        for rule in rules {
            array.push(rule.as_str());
        }
        let mut document = self.document.clone();
        document[list.key()] = value(array);
        self.replace_document(&document)
    }

    fn replace_document(&mut self, document: &DocumentMut) -> std::result::Result<(), PresetError> {
        let rendered = document.to_string();
        *self = Self::parse(&rendered)?;
        Ok(())
    }
}

impl Preset {
    /// Resolves exact names and disable patterns against one scanned plugin-name set.
    ///
    /// # Errors
    ///
    /// Returns a conflict when two actions select the same plugin or the plugin set contains duplicate names.
    pub fn resolve(&self, plugin_names: &[String]) -> Result<PresetResolution> {
        let candidates = unique_candidates(plugin_names)?;
        let mut changes = BTreeMap::<String, PresetChange>::new();
        let mut unmatched = Vec::new();

        apply_exact_rules(
            &mut changes,
            &mut unmatched,
            &candidates,
            PresetAction::Enable,
            &self.enable,
        )?;
        apply_exact_rules(
            &mut changes,
            &mut unmatched,
            &candidates,
            PresetAction::Disable,
            &self.disable,
        )?;
        apply_exact_rules(
            &mut changes,
            &mut unmatched,
            &candidates,
            PresetAction::Clear,
            &self.clear,
        )?;

        let mut pattern_expansions = Vec::new();
        for pattern in &self.disable_matching {
            let matcher = compile_pattern(pattern).map_err(|error| ProductError::InvalidInput {
                message: error.to_string(),
            })?;
            let matched_names = candidates
                .values()
                .filter(|name| matcher.is_match(name))
                .cloned()
                .collect::<Vec<_>>();
            for name in &matched_names {
                record_change(
                    &mut changes,
                    name,
                    PresetAction::Disable,
                    PresetRuleMatch {
                        source: PresetRuleSource::Pattern,
                        rule: pattern.clone(),
                    },
                )?;
            }
            pattern_expansions.push(PresetPatternExpansion {
                pattern: pattern.clone(),
                matches: matched_names,
            });
        }

        Ok(PresetResolution {
            changes: changes.into_values().collect(),
            pattern_expansions,
            unmatched,
        })
    }
}

/// Returns `%APPDATA%\Unclean\presets` when the process provides `APPDATA`.
#[must_use]
pub fn default_preset_directory() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("Unclean").join("presets"))
}

/// Returns the release or source-build preset directory available to the running executable.
#[must_use]
pub fn bundled_preset_directory() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|path| bundled_preset_directory_for(&path))
}

fn bundled_preset_directory_for(executable: &Path) -> Option<PathBuf> {
    let executable_directory = executable.parent()?;
    let adjacent = executable_directory.join("presets");
    if adjacent.is_dir() {
        return Some(adjacent);
    }

    let repository = executable_directory.parent()?.parent()?;
    let source_presets = repository.join("presets");
    (repository.join("Cargo.toml").is_file() && source_presets.is_dir()).then_some(source_presets)
}

/// Lists TOML files in one preset directory without creating the directory.
///
/// # Errors
///
/// Returns an error when preset directory loading fails.
pub fn list_presets(directory: Option<&Path>) -> Result<Vec<PresetFile>> {
    let Some(directory) = directory else {
        return Ok(Vec::new());
    };
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(directory).map_err(|error| preset_io_error(directory, &error))?;
    let mut presets = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| preset_io_error(directory, &error))?;
        let path = entry.path();
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
        {
            continue;
        }
        let Some(stem) = path.file_stem() else {
            continue;
        };
        presets.push(PresetFile {
            name: stem.to_string_lossy().into_owned(),
            path,
        });
    }
    presets.sort_by(|left, right| {
        normalize(&left.name)
            .cmp(&normalize(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(presets)
}

#[doc = "Lists user presets before bundled presets and keeps the first file for each name.\n\n# Errors\n\nReturns an error when available preset directory loading fails."]
pub fn list_available_presets(directory: Option<&Path>) -> Result<Vec<PresetFile>> {
    let bundled = bundled_preset_directory();
    list_presets_from_locations(directory, bundled.as_deref())
}

fn list_presets_from_locations(
    directory: Option<&Path>,
    bundled: Option<&Path>,
) -> Result<Vec<PresetFile>> {
    let mut presets = Vec::new();
    let mut names = BTreeSet::new();
    for location in [directory, bundled].into_iter().flatten() {
        for preset in list_presets(Some(location))? {
            if names.insert(normalize(&preset.name)) {
                presets.push(preset);
            }
        }
    }
    presets.sort_by(|left, right| {
        normalize(&left.name)
            .cmp(&normalize(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(presets)
}

#[doc = "Resolves a plain preset name from user or bundled presets and keeps path-like values explicit.\n\n# Errors\n\nReturns an error when a plain name has no available preset directory."]
pub fn resolve_preset_path(selector: &str, directory: Option<&Path>) -> Result<PathBuf> {
    let bundled = bundled_preset_directory();
    resolve_preset_path_from_locations(selector, directory, bundled.as_deref())
}

fn resolve_preset_path_from_locations(
    selector: &str,
    directory: Option<&Path>,
    bundled: Option<&Path>,
) -> Result<PathBuf> {
    if selector.trim().is_empty() {
        return Err(ProductError::InvalidInput {
            message: "the preset selector is empty".to_owned(),
        });
    }
    if is_path_like(selector) {
        return Ok(PathBuf::from(selector));
    }
    let mut fallback = None;
    for location in [directory, bundled].into_iter().flatten() {
        let candidate = location.join(format!("{selector}.toml"));
        if fallback.is_none() {
            fallback = Some(candidate.clone());
        }
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let Some(path) = fallback else {
        return Err(ProductError::InvalidInput {
            message: "No preset directory is available. Pass an explicit preset path and retry."
                .to_owned(),
        });
    };
    Ok(path)
}

/// Loads and validates one preset selected by name or path.
///
/// # Errors
///
/// Returns an error when selector resolution, file loading, or preset validation fails.
pub fn load_preset(selector: &str, directory: Option<&Path>) -> Result<(PathBuf, PresetDocument)> {
    let path = resolve_preset_path(selector, directory)?;
    let source = fs::read_to_string(&path).map_err(|error| preset_io_error(&path, &error))?;
    let document = PresetDocument::parse(&source).map_err(|error| ProductError::InvalidInput {
        message: format!("{} contains {error}", path.display()),
    })?;
    let reported_path = fs::canonicalize(&path).unwrap_or(path);
    Ok((reported_path, document))
}

/// Saves one validated preset through a synced same-directory replacement.
///
/// # Errors
///
/// Returns an error when the path is not TOML or the directory, write, sync, or replacement fails.
pub fn save_preset(path: &Path, document: &PresetDocument) -> Result<PathBuf> {
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
    {
        return Err(ProductError::InvalidInput {
            message: "preset save path must use the .toml extension".to_owned(),
        });
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| preset_io_error(parent, &error))?;
    let counter = PRESET_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".unclean-preset-{}-{counter}.tmp", process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| preset_io_error(&temporary, &error))?;
    let write_result = file
        .write_all(document.render().as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(preset_io_error(&temporary, &error));
    }
    let replace_result = if path.exists() {
        replace_file(path, &temporary)
    } else {
        install_file(path, &temporary)
    };
    if let Err(error) = replace_result {
        let _ = fs::remove_file(&temporary);
        return Err(preset_io_error(path, &error));
    }
    fs::canonicalize(path).map_err(|error| preset_io_error(path, &error))
}

fn parse_preset(document: &DocumentMut) -> std::result::Result<Preset, PresetError> {
    const KEYS: [&str; 7] = [
        "schema",
        "name",
        "description",
        "enable",
        "disable",
        "clear",
        "disable_matching",
    ];
    for (key, _) in document.iter() {
        if !KEYS.contains(&key) {
            return invalid_schema(format!("field \"{key}\" is not supported by schema 1"));
        }
    }

    let schema = required_integer(document, "schema")?;
    if schema != PRESET_SCHEMA {
        return invalid_schema(format!(
            "this build supports preset schema {PRESET_SCHEMA}; file uses schema {schema}"
        ));
    }
    let name = required_string(document, "name")?;
    if name.trim().is_empty() {
        return invalid_schema("field \"name\" cannot be empty");
    }
    let description = optional_string(document, "description")?;
    let enable = string_array(document, "enable")?;
    let disable = string_array(document, "disable")?;
    let clear = string_array(document, "clear")?;
    let disable_matching = string_array(document, "disable_matching")?;
    validate_rule_lists(&enable, &disable, &clear, &disable_matching)?;

    Ok(Preset {
        schema,
        name,
        description,
        enable,
        disable,
        clear,
        disable_matching,
    })
}

fn required_integer(
    document: &DocumentMut,
    key: &'static str,
) -> std::result::Result<i64, PresetError> {
    document
        .get(key)
        .and_then(Item::as_integer)
        .ok_or_else(|| PresetError::InvalidSchema {
            message: format!("field \"{key}\" must be an integer"),
        })
}

fn required_string(
    document: &DocumentMut,
    key: &'static str,
) -> std::result::Result<String, PresetError> {
    document
        .get(key)
        .and_then(Item::as_str)
        .map(str::to_owned)
        .ok_or_else(|| PresetError::InvalidSchema {
            message: format!("field \"{key}\" must be a string"),
        })
}

fn optional_string(
    document: &DocumentMut,
    key: &'static str,
) -> std::result::Result<Option<String>, PresetError> {
    match document.get(key) {
        Some(item) => {
            item.as_str()
                .map(str::to_owned)
                .map(Some)
                .ok_or_else(|| PresetError::InvalidSchema {
                    message: format!("field \"{key}\" must be a string"),
                })
        }
        None => Ok(None),
    }
}

fn string_array(
    document: &DocumentMut,
    key: &'static str,
) -> std::result::Result<Vec<String>, PresetError> {
    let Some(item) = document.get(key) else {
        return Ok(Vec::new());
    };
    let Some(array) = item.as_array() else {
        return invalid_schema(format!("field \"{key}\" must be an array of strings"));
    };
    array
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| PresetError::InvalidSchema {
                    message: format!("field \"{key}\" must contain only strings"),
                })
        })
        .collect()
}

fn validate_rule_lists(
    enable: &[String],
    disable: &[String],
    clear: &[String],
    disable_matching: &[String],
) -> std::result::Result<(), PresetError> {
    let mut exact_actions = BTreeMap::<String, PresetAction>::new();
    for (action, rules) in [
        (PresetAction::Enable, enable),
        (PresetAction::Disable, disable),
        (PresetAction::Clear, clear),
    ] {
        let mut seen = BTreeSet::new();
        for rule in rules {
            validate_rule_text(rule)?;
            let normalized = normalize(rule);
            if !seen.insert(normalized.clone()) {
                return invalid_schema(format!(
                    "field \"{}\" repeats plugin \"{rule}\"",
                    action.as_str()
                ));
            }
            if let Some(previous) = exact_actions.insert(normalized, action)
                && previous != action
            {
                return invalid_schema(format!(
                    "plugin \"{rule}\" has both {} and {} actions",
                    previous.as_str(),
                    action.as_str()
                ));
            }
        }
    }

    let mut seen_patterns = BTreeSet::new();
    for pattern in disable_matching {
        validate_rule_text(pattern)?;
        if !seen_patterns.insert(normalize(pattern)) {
            return invalid_schema(format!(
                "field \"disable_matching\" repeats pattern \"{pattern}\""
            ));
        }
        compile_pattern(pattern)?;
    }
    Ok(())
}

fn validate_rule_text(rule: &str) -> std::result::Result<(), PresetError> {
    if rule.trim().is_empty() {
        return invalid_schema("plugin names and patterns cannot be empty");
    }
    Ok(())
}

fn compile_pattern(pattern: &str) -> std::result::Result<GlobMatcher, PresetError> {
    GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| PresetError::InvalidSchema {
            message: format!("pattern \"{pattern}\" is invalid: {error}"),
        })
}

fn unique_candidates(plugin_names: &[String]) -> Result<BTreeMap<String, String>> {
    let mut candidates = BTreeMap::new();
    for name in plugin_names {
        let normalized = normalize(name);
        if let Some(previous) = candidates.insert(normalized, name.clone()) {
            return Err(ProductError::Conflict {
                message: format!(
                    "plugin names {previous} and {name} are equal without case distinctions"
                ),
            });
        }
    }
    Ok(candidates)
}

fn apply_exact_rules(
    changes: &mut BTreeMap<String, PresetChange>,
    unmatched: &mut Vec<UnmatchedPresetRule>,
    candidates: &BTreeMap<String, String>,
    action: PresetAction,
    rules: &[String],
) -> Result<()> {
    for rule in rules {
        let Some(name) = candidates.get(&normalize(rule)) else {
            unmatched.push(UnmatchedPresetRule {
                action,
                rule: rule.clone(),
            });
            continue;
        };
        record_change(
            changes,
            name,
            action,
            PresetRuleMatch {
                source: PresetRuleSource::Exact,
                rule: rule.clone(),
            },
        )?;
    }
    Ok(())
}

fn record_change(
    changes: &mut BTreeMap<String, PresetChange>,
    plugin: &str,
    action: PresetAction,
    matched_by: PresetRuleMatch,
) -> Result<()> {
    let key = normalize(plugin);
    if let Some(change) = changes.get_mut(&key) {
        if change.action != action {
            return Err(ProductError::Conflict {
                message: format!(
                    "preset rules select {} for both {} and {}",
                    change.plugin,
                    change.action.as_str(),
                    action.as_str()
                ),
            });
        }
        change.matched_by.push(matched_by);
        return Ok(());
    }
    changes.insert(
        key,
        PresetChange {
            plugin: plugin.to_owned(),
            action,
            matched_by: vec![matched_by],
        },
    );
    Ok(())
}

fn normalize(value: &str) -> String {
    value.to_ascii_lowercase()
}

fn invalid_schema<T>(message: impl Into<String>) -> std::result::Result<T, PresetError> {
    Err(PresetError::InvalidSchema {
        message: message.into(),
    })
}

fn is_path_like(selector: &str) -> bool {
    let path = Path::new(selector);
    path.is_absolute()
        || selector.contains('/')
        || selector.contains('\\')
        || selector.starts_with('.')
        || path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
}

fn preset_io_error(path: &Path, error: &std::io::Error) -> ProductError {
    match error.kind() {
        ErrorKind::NotFound => ProductError::NotFound {
            item: format!("preset file or directory {}", path.display()),
        },
        ErrorKind::PermissionDenied => ProductError::PermissionDenied {
            message: format!("Unclean cannot access preset path {}", path.display()),
        },
        _ => ProductError::InvalidInput {
            message: format!("preset path access failed at {}: {error}", path.display()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use tempfile::tempdir;

    use super::{
        PresetAction, PresetDocument, PresetRuleList, bundled_preset_directory_for,
        default_preset_directory, list_presets, list_presets_from_locations, load_preset,
        resolve_preset_path, resolve_preset_path_from_locations, save_preset,
    };

    const REVIEW_FIRST_PRESET: &str = include_str!("../../../presets/review-first.toml");
    const PROJECT_FIRST_PRESET: &str = include_str!("../../../presets/project-first.toml");
    const WINDOWS_DESKTOP_LEAN_PRESET: &str =
        include_str!("../../../presets/windows-desktop-lean.toml");

    const PRESET: &str = r#"
# Keep this ownership note.
schema = 1

# Keep this name note.
name = "Invented baseline"
description = "Synthetic preset for tests."
enable = ["CorePlugin"]
disable = ["UnusedPlugin"]
# Keep this clear note.
clear = []
disable_matching = ["Android*"]
"#;

    #[test]
    fn shipped_starter_presets_are_valid() -> Result<(), Box<dyn std::error::Error>> {
        for source in [REVIEW_FIRST_PRESET, PROJECT_FIRST_PRESET] {
            let document = PresetDocument::parse(source)?;
            let preset = document.preset();
            assert!(preset.enable.is_empty());
            assert!(preset.disable.is_empty());
            assert!(preset.clear.is_empty());
            assert!(preset.disable_matching.is_empty());
        }
        let document = PresetDocument::parse(WINDOWS_DESKTOP_LEAN_PRESET)?;
        let preset = document.preset();
        assert!(preset.enable.is_empty());
        assert!(preset.clear.is_empty());
        assert!(preset.disable_matching.is_empty());
        assert!(preset.disable.contains(&"AndroidFileServer".to_owned()));
        assert!(preset.disable.contains(&"N10XSourceCodeAccess".to_owned()));
        assert!(preset.disable.contains(&"OpenXR".to_owned()));
        assert!(
            !preset
                .disable
                .contains(&"VisualStudioSourceCodeAccess".to_owned())
        );
        Ok(())
    }

    #[test]
    fn source_builds_find_repository_presets() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let release = temp.path().join("target").join("release");
        let presets = temp.path().join("presets");
        fs::create_dir_all(&release)?;
        fs::create_dir_all(&presets)?;
        fs::write(temp.path().join("Cargo.toml"), "[workspace]\n")?;

        assert_eq!(
            bundled_preset_directory_for(&release.join("unclean-gui.exe")),
            Some(presets)
        );
        Ok(())
    }

    #[test]
    fn release_presets_take_precedence_over_source_presets() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let release = temp.path().join("target").join("release");
        let adjacent = release.join("presets");
        fs::create_dir_all(&adjacent)?;
        fs::create_dir_all(temp.path().join("presets"))?;
        fs::write(temp.path().join("Cargo.toml"), "[workspace]\n")?;

        assert_eq!(
            bundled_preset_directory_for(&release.join("unclean-gui.exe")),
            Some(adjacent)
        );
        Ok(())
    }

    #[test]
    fn user_presets_override_bundled_presets_with_the_same_name() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let user = temp.path().join("user");
        let bundled = temp.path().join("bundled");
        fs::create_dir_all(&user)?;
        fs::create_dir_all(&bundled)?;
        fs::write(user.join("lean.toml"), "user")?;
        fs::write(bundled.join("lean.toml"), "bundled")?;
        fs::write(bundled.join("review.toml"), "bundled")?;

        let presets = list_presets_from_locations(Some(&user), Some(&bundled))?;
        assert_eq!(presets.len(), 2);
        assert_eq!(
            presets
                .iter()
                .find(|preset| preset.name == "lean")
                .map(|preset| preset.path.as_path()),
            Some(user.join("lean.toml").as_path())
        );
        Ok(())
    }

    #[test]
    fn preset_name_resolution_falls_back_to_the_bundle() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let user = temp.path().join("user");
        let bundled = temp.path().join("bundled");
        fs::create_dir_all(&user)?;
        fs::create_dir_all(&bundled)?;
        let bundled_preset = bundled.join("lean.toml");
        fs::write(&bundled_preset, "bundled")?;

        assert_eq!(
            resolve_preset_path_from_locations("lean", Some(&user), Some(&bundled))?,
            bundled_preset
        );
        let user_preset = user.join("lean.toml");
        fs::write(&user_preset, "user")?;
        assert_eq!(
            resolve_preset_path_from_locations("lean", Some(&user), Some(&bundled))?,
            user_preset
        );
        Ok(())
    }

    #[test]
    fn comments_survive_application_authored_changes() -> Result<(), Box<dyn Error>> {
        let mut document = PresetDocument::parse(PRESET)?;
        document.set_name("Invented revised baseline")?;
        document.set_description(Some("Revised synthetic preset."))?;
        document.set_rules(PresetRuleList::Clear, &["LegacyPlugin".to_owned()])?;
        let rendered = document.render();

        assert!(rendered.contains("# Keep this ownership note."));
        assert!(rendered.contains("# Keep this name note."));
        assert!(rendered.contains("# Keep this clear note."));
        assert!(rendered.contains("name = \"Invented revised baseline\""));
        assert!(rendered.contains("description = \"Revised synthetic preset.\""));
        assert!(rendered.contains("clear = [\"LegacyPlugin\"]"));
        Ok(())
    }

    #[test]
    fn new_presets_save_and_replace_through_the_validated_document() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let path = temp.path().join("presets").join("invented.toml");
        let mut document = PresetDocument::new("Invented")?;
        document.set_rules(PresetRuleList::Enable, &["CorePlugin".to_owned()])?;

        let saved = save_preset(&path, &document)?;
        let (_, loaded) = load_preset(saved.to_string_lossy().as_ref(), None)?;
        assert_eq!(loaded.preset().enable, ["CorePlugin"]);

        document.set_rules(PresetRuleList::Enable, &["RevisedPlugin".to_owned()])?;
        save_preset(&path, &document)?;
        assert!(fs::read_to_string(path)?.contains("RevisedPlugin"));
        Ok(())
    }

    #[test]
    fn conflicting_glob_matches_are_rejected() -> Result<(), Box<dyn Error>> {
        let source = PRESET.replace("enable = [\"CorePlugin\"]", "enable = [\"AndroidMedia\"]");
        let document = PresetDocument::parse(&source)?;

        let error = document
            .preset()
            .resolve(&["AndroidMedia".to_owned(), "AndroidRuntime".to_owned()])
            .err()
            .ok_or("preset conflict was not reported")?;

        assert!(error.to_string().contains("both enable and disable"));
        Ok(())
    }

    #[test]
    fn patterns_expand_to_the_complete_reviewed_match_list() -> Result<(), Box<dyn Error>> {
        let document = PresetDocument::parse(PRESET)?;
        let resolution = document.preset().resolve(&[
            "UnusedPlugin".to_owned(),
            "AndroidRuntime".to_owned(),
            "CorePlugin".to_owned(),
            "AndroidMedia".to_owned(),
        ])?;

        assert_eq!(
            resolution.pattern_expansions[0].matches,
            ["AndroidMedia", "AndroidRuntime"]
        );
        assert_eq!(resolution.changes.len(), 4);
        assert_eq!(
            resolution
                .changes
                .iter()
                .find(|change| change.plugin == "CorePlugin")
                .ok_or("CorePlugin change is missing")?
                .action,
            PresetAction::Enable
        );
        Ok(())
    }

    #[test]
    fn unmatched_exact_names_keep_presets_portable_across_versions() -> Result<(), Box<dyn Error>> {
        let document = PresetDocument::parse(PRESET)?;
        let older = document
            .preset()
            .resolve(&["CorePlugin".to_owned(), "AndroidRuntime".to_owned()])?;
        let newer = document.preset().resolve(&[
            "CorePlugin".to_owned(),
            "UnusedPlugin".to_owned(),
            "AndroidRuntime".to_owned(),
        ])?;

        assert_eq!(older.unmatched[0].rule, "UnusedPlugin");
        assert!(newer.unmatched.is_empty());
        assert_eq!(older.pattern_expansions[0].matches, ["AndroidRuntime"]);
        assert_eq!(newer.pattern_expansions[0].matches, ["AndroidRuntime"]);
        Ok(())
    }

    #[test]
    fn names_resolve_in_the_supplied_directory_and_paths_remain_explicit()
    -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let directory = temp.path().join("presets");
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("invented.toml"), PRESET)?;

        assert_eq!(
            resolve_preset_path("invented", Some(&directory))?,
            directory.join("invented.toml")
        );
        assert_eq!(
            resolve_preset_path("./custom.toml", Some(&directory))?,
            std::path::PathBuf::from("./custom.toml")
        );
        let listed = list_presets(Some(&directory))?;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "invented");
        let (_, loaded) = load_preset("invented", Some(&directory))?;
        assert_eq!(loaded.preset().name, "Invented baseline");
        Ok(())
    }

    #[test]
    fn default_directory_is_optional_outside_the_windows_environment() {
        let _ = default_preset_directory();
    }
}
