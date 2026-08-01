//! Discovers engine installations and reports their identity and health without changing files.

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::ffi::OsStr;

use crate::{Error, Result};

const PARTIAL_DESCRIPTOR_FLOOR: usize = 100;
const BUILD_VERSION_PATH: [&str; 3] = ["Engine", "Build", "Build.version"];
const PLUGIN_DIRECTORY_PATH: [&str; 2] = ["Engine", "Plugins"];

/// Identifies the source that supplied an engine candidate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// Uses a path supplied through an explicit command option.
    Explicit,
    /// Uses an engine root inferred from the current working directory.
    WorkingDirectory,
    /// Uses an `InstalledDirectory` value from the Windows registry.
    Registry,
    /// Uses an engine artifact from the Epic Games Launcher manifest.
    Launcher,
}

impl DiscoverySource {
    /// Returns the stable source name used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::WorkingDirectory => "working_directory",
            Self::Registry => "registry",
            Self::Launcher => "launcher",
        }
    }

    const fn priority(self) -> u8 {
        match self {
            Self::Explicit => 0,
            Self::WorkingDirectory => 1,
            Self::Registry => 2,
            Self::Launcher => 3,
        }
    }
}

/// Identifies one stable engine-health finding.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineIssueCode {
    /// Reports a missing installation directory.
    MissingInstallation,
    /// Reports a missing Engine\Plugins directory.
    MissingPluginDirectory,
    /// Reports an installation with fewer than 100 plugin descriptors.
    LowDescriptorCount,
    /// Reports a missing Engine\Build\Build.version file.
    MissingBuildMetadata,
    /// Reports unreadable or invalid build metadata.
    InvalidBuildMetadata,
    /// Reports a directory that stopped the plugin descriptor scan.
    PluginScanFailed,
}

impl EngineIssueCode {
    /// Returns the stable issue name used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingInstallation => "missing_installation",
            Self::MissingPluginDirectory => "missing_plugin_directory",
            Self::LowDescriptorCount => "low_descriptor_count",
            Self::MissingBuildMetadata => "missing_build_metadata",
            Self::InvalidBuildMetadata => "invalid_build_metadata",
            Self::PluginScanFailed => "plugin_scan_failed",
        }
    }
}

/// Describes one engine-health finding and its recovery action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EngineIssue {
    /// Identifies the finding for machine output.
    pub code: EngineIssueCode,
    /// Explains the finding and the action needed before a write.
    pub message: String,
}

/// Classifies whether an engine installation can support later operations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineHealth {
    /// Provides build metadata and at least 100 plugin descriptors.
    Healthy,
    /// Provides a plugin directory but needs review before a write.
    Partial,
    /// Lacks the installation root or plugin directory required for selection.
    Unavailable,
}

impl EngineHealth {
    /// Returns the stable health name used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
        }
    }

    /// Reports whether later commands may select the installation.
    #[must_use]
    pub const fn is_selectable(self) -> bool {
        !matches!(self, Self::Unavailable)
    }
}

/// Reports one canonical engine installation and its read-only health result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EngineInstallation {
    /// Identifies the installation through its canonical absolute path.
    pub path: PathBuf,
    /// Displays the version read from Build.version or the discovery source.
    pub version: Option<String>,
    /// Identifies the highest-priority source that reported this path.
    pub source: DiscoverySource,
    /// Classifies the installation for selection and review.
    pub health: EngineHealth,
    /// Counts .uplugin files visible under Engine\Plugins.
    pub descriptor_count: usize,
    /// Lists the findings that produced the health classification.
    pub issues: Vec<EngineIssue>,
}

/// Reports one discovery-source failure without hiding usable engines from other sources.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiscoveryWarning {
    /// Identifies the unreadable source.
    pub source: DiscoverySource,
    /// Explains the failure and the available recovery action.
    pub message: String,
}

/// Returns the merged discovery result and non-fatal source warnings.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DiscoveryReport {
    /// Lists engine installations by descending version and canonical path.
    pub engines: Vec<EngineInstallation>,
    /// Lists source failures that did not stop discovery.
    pub warnings: Vec<DiscoveryWarning>,
}

/// Selects which read-only discovery sources contribute candidates.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    /// Adds paths supplied by a command or saved application state.
    pub explicit_paths: Vec<PathBuf>,
    /// Adds the containing engine when this directory sits under an engine root.
    pub current_dir: Option<PathBuf>,
    /// Reads engine artifacts from this launcher manifest when present.
    pub launcher_manifest: Option<PathBuf>,
    /// Reads installed engine paths from the Windows registry.
    pub include_registry: bool,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            explicit_paths: Vec::new(),
            current_dir: env::current_dir().ok(),
            launcher_manifest: {
                #[cfg(windows)]
                {
                    Some(default_launcher_manifest())
                }
                #[cfg(not(windows))]
                {
                    None
                }
            },
            include_registry: true,
        }
    }
}

#[derive(Clone, Debug)]
struct EngineCandidate {
    path: PathBuf,
    source: DiscoverySource,
    version_hint: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BuildVersion {
    #[serde(rename = "MajorVersion")]
    major: u32,
    #[serde(rename = "MinorVersion")]
    minor: u32,
    #[serde(rename = "PatchVersion")]
    patch: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LauncherManifest {
    #[serde(default)]
    installation_list: Vec<LauncherEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LauncherEntry {
    #[serde(default)]
    install_location: String,
    #[serde(default)]
    namespace_id: String,
    #[serde(default)]
    artifact_id: String,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    app_name: String,
}

#[derive(Debug)]
struct DescriptorScan {
    count: usize,
    first_error: Option<(PathBuf, io::Error)>,
}

/// Discovers, merges, validates, and sorts engine installations from the selected sources.
#[must_use]
pub fn discover_engines(options: &DiscoveryOptions) -> DiscoveryReport {
    let base_dir = options
        .current_dir
        .clone()
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut candidates = options
        .explicit_paths
        .iter()
        .map(|path| {
            make_candidate(
                path,
                &base_dir,
                DiscoverySource::Explicit,
                version_hint_from_path(path),
            )
        })
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();

    if options.include_registry {
        let registry = read_registry_candidates(&base_dir);
        candidates.extend(registry.0);
        warnings.extend(registry.1);
    }

    if let Some(manifest_path) = &options.launcher_manifest {
        let launcher = read_launcher_candidates(manifest_path, &base_dir);
        candidates.extend(launcher.0);
        warnings.extend(launcher.1);
    }

    if let Some(current_dir) = &options.current_dir
        && let Some(root) = infer_engine_root(current_dir)
    {
        candidates.push(make_candidate(
            &root,
            &base_dir,
            DiscoverySource::WorkingDirectory,
            None,
        ));
    }

    let mut engines = inspect_candidates(
        merge_candidates(candidates)
            .into_values()
            .collect::<Vec<_>>(),
    );
    engines.sort_by_key(|engine| {
        (
            Reverse(version_tuple(engine.version.as_deref())),
            path_identity_key(&engine.path),
        )
    });

    DiscoveryReport { engines, warnings }
}

fn inspect_candidates(candidates: Vec<EngineCandidate>) -> Vec<EngineInstallation> {
    const MAX_DISCOVERY_WORKERS: usize = 4;
    let worker_count = candidates.len().min(MAX_DISCOVERY_WORKERS);
    if worker_count < 2 {
        return candidates.into_iter().map(inspect_candidate).collect();
    }
    let mut buckets = vec![Vec::new(); worker_count];
    for (index, candidate) in candidates.into_iter().enumerate() {
        buckets[index % worker_count].push(candidate);
    }
    std::thread::scope(|scope| {
        let handles = buckets
            .iter()
            .map(|bucket| {
                let worker_bucket = bucket.clone();
                scope.spawn(move || {
                    worker_bucket
                        .into_iter()
                        .map(inspect_candidate)
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let mut engines = Vec::new();
        for (bucket, handle) in buckets.into_iter().zip(handles) {
            match handle.join() {
                Ok(worker_engines) => engines.extend(worker_engines),
                Err(_) => engines.extend(bucket.into_iter().map(inspect_candidate)),
            }
        }
        engines
    })
}

/// Finds one engine by display version and rejects ambiguous version matches.
pub fn select_engine_by_version<'a>(
    engines: &'a [EngineInstallation],
    selector: &str,
) -> Result<&'a EngineInstallation> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(Error::InvalidInput {
            message: "the engine version selector is empty".to_owned(),
        });
    }

    let matches = engines
        .iter()
        .filter(|engine| {
            engine.health.is_selectable()
                && engine
                    .version
                    .as_deref()
                    .is_some_and(|version| version_matches(version, selector))
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Err(Error::NotFound {
            item: format!("engine version {selector}"),
        }),
        [engine] => Ok(*engine),
        _ => Err(Error::AmbiguousEngine {
            selector: selector.to_owned(),
            candidates: matches
                .iter()
                .map(|engine| engine.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// Walks parent directories until build metadata and the plugin directory identify an engine.
#[must_use]
pub fn infer_engine_root(start: &Path) -> Option<PathBuf> {
    let mut candidate = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        if joined(&candidate, &BUILD_VERSION_PATH).is_file()
            && joined(&candidate, &PLUGIN_DIRECTORY_PATH).is_dir()
        {
            let base_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            return Some(canonical_or_lexical(&candidate, &base_dir));
        }
        if !candidate.pop() {
            return None;
        }
    }
}

fn read_launcher_candidates(
    manifest_path: &Path,
    base_dir: &Path,
) -> (Vec<EngineCandidate>, Vec<DiscoveryWarning>) {
    if !manifest_path.exists() {
        return (Vec::new(), Vec::new());
    }

    let text = match fs::read_to_string(manifest_path) {
        Ok(text) => text,
        Err(error) => {
            return (
                Vec::new(),
                vec![DiscoveryWarning {
                    source: DiscoverySource::Launcher,
                    message: format!(
                        "Launcher discovery read failed at {}: {error}. Check file permissions or pass --engine-path.",
                        manifest_path.display()
                    ),
                }],
            );
        }
    };

    match launcher_candidates_from_str(&text, base_dir) {
        Ok(candidates) => (candidates, Vec::new()),
        Err(error) => (
            Vec::new(),
            vec![DiscoveryWarning {
                source: DiscoverySource::Launcher,
                message: format!(
                    "Launcher discovery parse failed at {}: {error}. Repair the launcher manifest or pass --engine-path.",
                    manifest_path.display()
                ),
            }],
        ),
    }
}

fn launcher_candidates_from_str(
    text: &str,
    base_dir: &Path,
) -> std::result::Result<Vec<EngineCandidate>, serde_json::Error> {
    let manifest: LauncherManifest = serde_json::from_str(text)?;
    Ok(manifest
        .installation_list
        .into_iter()
        .filter(is_engine_artifact)
        .filter(|entry| !entry.install_location.trim().is_empty())
        .map(|entry| {
            make_candidate(
                Path::new(entry.install_location.trim()),
                base_dir,
                DiscoverySource::Launcher,
                version_hint_from_launcher(&entry.app_version),
            )
        })
        .collect())
}

fn is_engine_artifact(entry: &LauncherEntry) -> bool {
    entry.namespace_id.eq_ignore_ascii_case("ue")
        && (looks_like_engine_name(&entry.app_name) || looks_like_engine_name(&entry.artifact_id))
}

fn looks_like_engine_name(value: &str) -> bool {
    let Some(version) = value.strip_prefix("UE_") else {
        return false;
    };
    version
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
}

fn version_hint_from_launcher(value: &str) -> Option<String> {
    let version = value.split('-').next()?.trim();
    if version.contains('.')
        && version
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        Some(version.to_owned())
    } else {
        None
    }
}

fn version_hint_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    name.strip_prefix("UE_").map(ToOwned::to_owned)
}

fn make_candidate(
    path: &Path,
    base_dir: &Path,
    source: DiscoverySource,
    version_hint: Option<String>,
) -> EngineCandidate {
    EngineCandidate {
        path: canonical_or_lexical(path, base_dir),
        source,
        version_hint,
    }
}

fn merge_candidates(candidates: Vec<EngineCandidate>) -> BTreeMap<String, EngineCandidate> {
    let mut merged = BTreeMap::<String, EngineCandidate>::new();
    for candidate in candidates {
        let key = path_identity_key(&candidate.path);
        match merged.get_mut(&key) {
            Some(existing) => {
                if candidate.source.priority() < existing.source.priority() {
                    existing.source = candidate.source;
                }
                if hint_quality(candidate.version_hint.as_deref())
                    > hint_quality(existing.version_hint.as_deref())
                {
                    existing.version_hint = candidate.version_hint;
                }
            }
            None => {
                merged.insert(key, candidate);
            }
        }
    }
    merged
}

fn hint_quality(hint: Option<&str>) -> usize {
    hint.map_or(0, |value| value.split('.').count())
}

fn inspect_candidate(candidate: EngineCandidate) -> EngineInstallation {
    let mut issues = Vec::new();
    if !candidate.path.is_dir() {
        issues.push(EngineIssue {
            code: EngineIssueCode::MissingInstallation,
            message: "The installation directory is missing. Repair the installation or remove its discovery entry.".to_owned(),
        });
        return EngineInstallation {
            path: candidate.path,
            version: candidate.version_hint,
            source: candidate.source,
            health: EngineHealth::Unavailable,
            descriptor_count: 0,
            issues,
        };
    }

    let build_version_path = joined(&candidate.path, &BUILD_VERSION_PATH);
    let version = match fs::read_to_string(&build_version_path) {
        Ok(text) => match serde_json::from_str::<BuildVersion>(&text) {
            Ok(build) => Some(format!("{}.{}.{}", build.major, build.minor, build.patch)),
            Err(error) => {
                issues.push(EngineIssue {
                    code: EngineIssueCode::InvalidBuildMetadata,
                    message: format!(
                        "Build metadata parse failed: {error}. Repair {} before selecting this engine.",
                        build_version_path.display()
                    ),
                });
                candidate.version_hint
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            issues.push(EngineIssue {
                code: EngineIssueCode::MissingBuildMetadata,
                message: "Engine\\Build\\Build.version is missing. Select this folder only if it is a source build.".to_owned(),
            });
            candidate.version_hint
        }
        Err(error) => {
            issues.push(EngineIssue {
                code: EngineIssueCode::InvalidBuildMetadata,
                message: format!(
                    "Build metadata read failed: {error}. Check {} permissions before selecting this engine.",
                    build_version_path.display()
                ),
            });
            candidate.version_hint
        }
    };

    let plugin_dir = joined(&candidate.path, &PLUGIN_DIRECTORY_PATH);
    if !plugin_dir.is_dir() {
        issues.push(EngineIssue {
            code: EngineIssueCode::MissingPluginDirectory,
            message:
                "Engine\\Plugins is missing. Repair the installation before selecting this engine."
                    .to_owned(),
        });
        return EngineInstallation {
            path: candidate.path,
            version,
            source: candidate.source,
            health: EngineHealth::Unavailable,
            descriptor_count: 0,
            issues,
        };
    }

    let scan = count_plugin_descriptors(&plugin_dir);
    if scan.count < PARTIAL_DESCRIPTOR_FLOOR {
        issues.push(EngineIssue {
            code: EngineIssueCode::LowDescriptorCount,
            message: format!(
                "The plugin scan found {} descriptors. Review the installation before writing.",
                scan.count
            ),
        });
    }
    if let Some((path, error)) = scan.first_error {
        issues.push(EngineIssue {
            code: EngineIssueCode::PluginScanFailed,
            message: format!(
                "Plugin scan read failed at {}: {error}. Check folder permissions before writing.",
                path.display()
            ),
        });
    }

    let health = if issues.is_empty() {
        EngineHealth::Healthy
    } else {
        EngineHealth::Partial
    };

    EngineInstallation {
        path: candidate.path,
        version,
        source: candidate.source,
        health,
        descriptor_count: scan.count,
        issues,
    }
}

fn count_plugin_descriptors(root: &Path) -> DescriptorScan {
    let mut stack = vec![root.to_path_buf()];
    let mut count = 0;
    let mut first_error = None;

    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some((directory, error));
                }
                continue;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some((directory.clone(), error));
                    }
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some((entry.path(), error));
                    }
                    continue;
                }
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("uplugin"))
            {
                count += 1;
            }
        }
    }

    DescriptorScan { count, first_error }
}

fn joined<const N: usize>(root: &Path, components: &[&str; N]) -> PathBuf {
    components
        .iter()
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

fn canonical_or_lexical(path: &Path, base_dir: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };
    fs::canonicalize(&absolute)
        .map_or_else(|_| lexical_normalize(&absolute), simplify_windows_prefix)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(windows)]
fn simplify_windows_prefix(path: PathBuf) -> PathBuf {
    let display = path.to_string_lossy();
    if let Some(network_path) = display.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{network_path}"))
    } else if let Some(drive_path) = display.strip_prefix(r"\\?\") {
        PathBuf::from(drive_path)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn simplify_windows_prefix(path: PathBuf) -> PathBuf {
    path
}

fn path_identity_key(path: &Path) -> String {
    if cfg!(windows) {
        path.to_string_lossy().replace('/', "\\").to_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn version_tuple(version: Option<&str>) -> (u32, u32, u32) {
    let mut parts = version
        .into_iter()
        .flat_map(|value| value.split('.'))
        .filter_map(|part| part.parse::<u32>().ok());
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

fn version_matches(version: &str, selector: &str) -> bool {
    version.eq_ignore_ascii_case(selector)
        || version
            .strip_prefix(selector)
            .is_some_and(|remainder| remainder.starts_with('.'))
}

#[cfg(windows)]
fn default_launcher_manifest() -> PathBuf {
    let program_data =
        env::var_os("PROGRAMDATA").unwrap_or_else(|| OsStr::new(r"C:\ProgramData").to_owned());
    PathBuf::from(program_data)
        .join("Epic")
        .join("UnrealEngineLauncher")
        .join("LauncherInstalled.dat")
}

#[cfg(windows)]
fn read_registry_candidates(base_dir: &Path) -> (Vec<EngineCandidate>, Vec<DiscoveryWarning>) {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};

    let local_machine = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = match local_machine.open_subkey_with_flags(
        r"SOFTWARE\EpicGames\Unreal Engine",
        KEY_READ | KEY_WOW64_64KEY,
    ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (Vec::new(), Vec::new());
        }
        Err(error) => {
            return (
                Vec::new(),
                vec![DiscoveryWarning {
                    source: DiscoverySource::Registry,
                    message: format!(
                        "Registry discovery failed: {error}. Check access to HKLM\\SOFTWARE\\EpicGames\\Unreal Engine or pass --engine-path."
                    ),
                }],
            );
        }
    };

    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    for key_name in key.enum_keys() {
        let key_name = match key_name {
            Ok(key_name) => key_name,
            Err(error) => {
                warnings.push(DiscoveryWarning {
                    source: DiscoverySource::Registry,
                    message: format!(
                        "Registry discovery failed to read an engine key: {error}. Check registry permissions or pass --engine-path."
                    ),
                });
                continue;
            }
        };
        let installed_directory = key
            .open_subkey_with_flags(&key_name, KEY_READ | KEY_WOW64_64KEY)
            .and_then(|engine_key| engine_key.get_value::<String, _>("InstalledDirectory"))
            .ok();
        entries.push((key_name, installed_directory));
    }

    let registry = registry_candidates_from_entries(entries, base_dir);
    warnings.extend(registry.1);
    (registry.0, warnings)
}

#[cfg(not(windows))]
fn read_registry_candidates(_base_dir: &Path) -> (Vec<EngineCandidate>, Vec<DiscoveryWarning>) {
    (Vec::new(), Vec::new())
}

#[cfg(any(windows, test))]
fn registry_candidates_from_entries(
    entries: impl IntoIterator<Item = (String, Option<String>)>,
    base_dir: &Path,
) -> (Vec<EngineCandidate>, Vec<DiscoveryWarning>) {
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    for (version, installed_directory) in entries {
        match installed_directory {
            Some(path) if !path.trim().is_empty() => candidates.push(make_candidate(
                Path::new(path.trim()),
                base_dir,
                DiscoverySource::Registry,
                Some(version),
            )),
            _ => warnings.push(DiscoveryWarning {
                source: DiscoverySource::Registry,
                message: format!(
                    "Registry entry {version} has no InstalledDirectory value. Repair the entry or pass --engine-path."
                ),
            }),
        }
    }
    (candidates, warnings)
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::path::{Path, PathBuf};
    use std::{fs, io};

    use tempfile::tempdir;

    use super::{
        DiscoveryOptions, DiscoverySource, EngineCandidate, EngineHealth, EngineInstallation,
        discover_engines, infer_engine_root, launcher_candidates_from_str, make_candidate,
        merge_candidates, registry_candidates_from_entries, select_engine_by_version,
    };

    fn create_engine(
        root: &Path,
        include_build_metadata: bool,
        descriptor_count: usize,
    ) -> io::Result<()> {
        let plugins = root.join("Engine").join("Plugins");
        fs::create_dir_all(&plugins)?;
        if include_build_metadata {
            let build = root.join("Engine").join("Build");
            fs::create_dir_all(&build)?;
            fs::write(
                build.join("Build.version"),
                r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":1}"#,
            )?;
        }
        for index in 0..descriptor_count {
            fs::write(plugins.join(format!("Plugin{index}.uplugin")), "{}")?;
        }
        Ok(())
    }

    #[test]
    fn launcher_parser_keeps_engine_artifacts_and_discards_plugin_artifacts()
    -> std::result::Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_5.8");
        let json = serde_json::json!({
            "InstallationList": [
                {
                    "InstallLocation": engine_path,
                    "NamespaceId": "ue",
                    "ArtifactId": "UE_5.8",
                    "AppVersion": "5.8.1-123+++UE5",
                    "AppName": "UE_5.8"
                },
                {
                    "InstallLocation": engine_path,
                    "NamespaceId": "ue",
                    "ArtifactId": "FabPlugin_5.8",
                    "AppVersion": "5.8.0-99",
                    "AppName": "FabPlugin_5.8"
                }
            ]
        })
        .to_string();

        let candidates = launcher_candidates_from_str(&json, temp.path())?;

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].source, DiscoverySource::Launcher);
        assert_eq!(candidates[0].version_hint.as_deref(), Some("5.8.1"));
        Ok(())
    }

    #[test]
    fn registry_entries_report_empty_values_and_merge_duplicate_paths() {
        let base = Path::new("C:\\");
        let (registry, warnings) = registry_candidates_from_entries(
            [
                ("5.8".to_owned(), Some("C:\\UE_5.8".to_owned())),
                ("5.7".to_owned(), None),
            ],
            base,
        );
        let launcher = EngineCandidate {
            path: registry[0].path.clone(),
            source: DiscoverySource::Launcher,
            version_hint: Some("5.8.1".to_owned()),
        };

        let merged = merge_candidates(vec![launcher, registry[0].clone()]);
        let candidate = merged.values().next();

        assert_eq!(warnings.len(), 1);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            candidate.map(|value| value.source),
            Some(DiscoverySource::Registry)
        );
        assert_eq!(
            candidate.and_then(|value| value.version_hint.as_deref()),
            Some("5.8.1")
        );
    }

    #[test]
    fn discovery_classifies_missing_partial_and_healthy_installations()
    -> std::result::Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let missing = temp.path().join("UE_5.6");
        let partial = temp.path().join("UE_5.7");
        let healthy = temp.path().join("UE_5.8");
        create_engine(&partial, false, 2)?;
        create_engine(&healthy, true, 100)?;

        let report = discover_engines(&DiscoveryOptions {
            explicit_paths: vec![missing, partial, healthy],
            current_dir: None,
            launcher_manifest: None,
            include_registry: false,
        });

        assert_eq!(report.engines.len(), 3);
        assert_eq!(report.engines[0].version.as_deref(), Some("5.8.1"));
        assert_eq!(report.engines[0].health, EngineHealth::Healthy);
        assert_eq!(report.engines[1].health, EngineHealth::Partial);
        assert_eq!(report.engines[1].descriptor_count, 2);
        assert_eq!(report.engines[2].health, EngineHealth::Unavailable);
        Ok(())
    }

    #[test]
    fn current_directory_inference_finds_source_build_roots()
    -> std::result::Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine = temp.path().join("SourceEngine");
        create_engine(&engine, true, 0)?;
        let nested = engine.join("Engine").join("Source").join("Runtime");
        fs::create_dir_all(&nested)?;

        let inferred = infer_engine_root(&nested);
        let report = discover_engines(&DiscoveryOptions {
            explicit_paths: Vec::new(),
            current_dir: Some(nested),
            launcher_manifest: None,
            include_registry: false,
        });

        assert_eq!(report.engines.len(), 1);
        assert_eq!(inferred.as_deref(), Some(report.engines[0].path.as_path()));
        assert_eq!(report.engines[0].source, DiscoverySource::WorkingDirectory);
        assert_eq!(report.engines[0].version.as_deref(), Some("5.8.1"));
        Ok(())
    }

    #[test]
    fn version_selection_reports_missing_and_ambiguous_matches() {
        let engine = |path: &str| EngineInstallation {
            path: PathBuf::from(path),
            version: Some("5.8.1".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 100,
            issues: Vec::new(),
        };
        let engines = [engine("C:\\UE_A"), engine("C:\\UE_B")];

        let ambiguous = select_engine_by_version(&engines, "5.8");
        let missing = select_engine_by_version(&engines, "5.7");

        assert!(ambiguous.is_err());
        assert_eq!(
            ambiguous.err().map(|error| error.code().exit_code()),
            Some(5)
        );
        assert_eq!(missing.err().map(|error| error.code().exit_code()), Some(4));
    }

    #[test]
    fn explicit_candidates_resolve_relative_paths_from_the_supplied_base() {
        let base = Path::new("C:\\Engines");
        let candidate = make_candidate(Path::new("UE_5.8"), base, DiscoverySource::Explicit, None);

        assert_eq!(candidate.path, base.join("UE_5.8"));
    }
}
