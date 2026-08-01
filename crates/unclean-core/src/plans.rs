//! Builds immutable reviewed plans from current bytes and resolved preset intent.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::dependencies::{DependencyWarning, analyze_plugins};
use crate::descriptors::{
    ByteSpan, DeclaredPluginState, DescriptorDocument, PluginDescriptor, PluginScanWarning,
    scan_engine_plugins,
};
use crate::discovery::EngineInstallation;
use crate::presets::{
    Preset, PresetAction, PresetPatternExpansion, PresetRuleMatch, UnmatchedPresetRule,
};
use crate::{Error, Result};

/// Identifies the plan schema emitted by this build.
pub const PLAN_SCHEMA: u8 = 1;

static OPERATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Supplies the backup root and unique operation identifier used by one plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanBuildOptions {
    backup_root: PathBuf,
    operation_id: String,
}

impl PlanBuildOptions {
    /// Validates an explicit backup root and operation identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the root is relative or the identifier could escape its directory.
    pub fn new(backup_root: PathBuf, operation_id: String) -> Result<Self> {
        if !backup_root.is_absolute() {
            return Err(Error::InvalidInput {
                message: format!("backup root is not absolute: {}", backup_root.display()),
            });
        }
        if !valid_operation_id(&operation_id) {
            return Err(Error::InvalidInput {
                message:
                    "operation identifier must contain only letters, digits, periods, hyphens, or underscores"
                        .to_owned(),
            });
        }
        Ok(Self {
            backup_root,
            operation_id,
        })
    }

    /// Creates options under `%APPDATA%\Unclean\backups` with a process-unique identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when `APPDATA` is missing or the system clock predates the Unix epoch.
    pub fn for_current_process() -> Result<Self> {
        let backup_root = default_backup_root()?;
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| Error::Internal {
                message: format!("the system clock cannot create an operation identifier: {error}"),
            })?;
        let counter = OPERATION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let operation_id = format!(
            "{}-{:09}-{}-{counter}",
            duration.as_secs(),
            duration.subsec_nanos(),
            process::id()
        );
        Self::new(backup_root, operation_id)
    }

    /// Returns the unique identifier reserved for the planned operation.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Returns the engine-specific backup directory reserved by these options.
    #[must_use]
    pub fn backup_directory(&self, engine: &EngineInstallation) -> PathBuf {
        self.backup_root
            .join(engine_storage_id(engine))
            .join(&self.operation_id)
    }

    /// Returns the project-specific backup directory reserved by these options.
    #[must_use]
    pub fn project_backup_directory(&self, project_path: &Path) -> PathBuf {
        let identity = normalized_path_identity(project_path);
        let hash = sha256_hex(identity.as_bytes());
        let label = project_path.file_stem().map_or_else(
            || "project".to_owned(),
            |value| safe_path_segment(&value.to_string_lossy()),
        );
        self.backup_root
            .join("projects")
            .join(format!("{label}-{}", &hash[..12]))
            .join(&self.operation_id)
    }
}

/// Identifies the only engine descriptor field an engine-mode plan may change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum PlanField {
    /// Changes or removes the plugin default declaration.
    EnabledByDefault,
}

impl PlanField {
    /// Returns the Unreal descriptor key used in review output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnabledByDefault => "EnabledByDefault",
        }
    }
}

/// Compares one impact count before and after the planned edits.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CountChange {
    /// Records the current count.
    pub before: usize,
    /// Records the planned count.
    pub after: usize,
}

/// Summarizes the effective plugin and module impact of one plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlanImpact {
    /// Counts descriptors explicitly enabled as dependency roots.
    pub default_roots: CountChange,
    /// Counts plugins enabled after dependency closure.
    pub effective_plugins: CountChange,
    /// Counts modules declared by effective plugins.
    pub declared_modules: CountChange,
}

/// Identifies why a resolved preset action does not need a byte edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanNoOpReason {
    /// Reports that the descriptor already declares the requested state.
    AlreadyDeclared,
    /// Reports that an absent declaration already avoids a default-enabled root.
    UnspecifiedAlreadyOff,
}

impl PlanNoOpReason {
    /// Returns the review text for the no-op condition.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::AlreadyDeclared => "The descriptor already declares the requested state.",
            Self::UnspecifiedAlreadyOff => {
                "The descriptor has no EnabledByDefault field, so no default root needs removal."
            }
        }
    }
}

/// Records one resolved preset action that leaves descriptor bytes unchanged.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanNoOp {
    /// Names the selected plugin.
    pub plugin: String,
    /// Records the requested preset action.
    pub action: PresetAction,
    /// Retains the declared state found during planning.
    pub declared_state: DeclaredPluginState,
    /// Reports the dependency-resolved state before the plan.
    pub effective_enabled: bool,
    /// Explains why a request needs no byte edit.
    pub reason: PlanNoOpReason,
    /// Lists every preset rule that selected the action.
    pub matched_by: Vec<PresetRuleMatch>,
}

/// Locates the smallest differing ranges in the source and planned byte streams.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedByteChange {
    /// Covers the changed source bytes and may be empty for an insertion.
    pub source: ByteSpan,
    /// Covers the replacement bytes and may be empty for a removal.
    pub planned: ByteSpan,
}

/// Stores one verified descriptor edit and the bytes needed by the protected writer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedFileEdit {
    /// Names the plugin selected by the preset.
    pub plugin: String,
    /// Records the absolute descriptor path reviewed by the frontend.
    pub path: PathBuf,
    /// Records the target path relative to the engine root.
    pub relative_path: PathBuf,
    /// Identifies the permitted descriptor field.
    pub field: PlanField,
    /// Retains the current declared state.
    pub value_before: DeclaredPluginState,
    /// Records the planned declared state.
    pub value_after: DeclaredPluginState,
    /// Hashes the exact source bytes read during planning.
    pub sha256_before: String,
    /// Hashes the exact verified planned bytes.
    pub sha256_after: String,
    /// Reports the planned output size without serializing descriptor content.
    pub planned_byte_count: usize,
    /// Locates the differing ranges in both byte streams.
    pub byte_change: PlannedByteChange,
    /// Lists every preset rule that selected the action.
    pub matched_by: Vec<PresetRuleMatch>,
    #[serde(skip)]
    planned_bytes: Vec<u8>,
}

impl PlannedFileEdit {
    /// Returns the verified output bytes retained by the immutable plan.
    #[must_use]
    pub fn planned_bytes(&self) -> &[u8] {
        &self.planned_bytes
    }
}

/// Reports roots that keep a requested plugin enabled after the descriptor edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanDependencyWarning {
    /// Names the requested plugin.
    pub plugin: String,
    /// Records the disable or clear action that triggered the warning.
    pub action: PresetAction,
    /// Lists every enabled root that reaches the plugin.
    pub roots: Vec<String>,
    /// Retains one stable root-to-plugin dependency path.
    pub effective_path: Vec<String>,
    /// States the remaining effective state and recovery action.
    pub message: String,
}

/// Identifies the preset source used to build a plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanPreset {
    /// Records the validated preset display name.
    pub name: String,
    /// Records the resolved preset file path.
    pub path: PathBuf,
}

/// Owns the complete read-only plan rendered by both frontends.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EnginePlan {
    schema: u8,
    operation_id: String,
    engine: EngineInstallation,
    preset: PlanPreset,
    backup_directory: PathBuf,
    impact: PlanImpact,
    changes: Vec<PlannedFileEdit>,
    no_ops: Vec<PlanNoOp>,
    dependency_warnings: Vec<PlanDependencyWarning>,
    graph_warnings: Vec<DependencyWarning>,
    scan_warnings: Vec<PluginScanWarning>,
    pattern_expansions: Vec<PresetPatternExpansion>,
    unmatched_rules: Vec<UnmatchedPresetRule>,
}

impl EnginePlan {
    /// Returns the machine schema for this plan.
    #[must_use]
    pub const fn schema(&self) -> u8 {
        self.schema
    }

    /// Returns the unique identifier reserved for this reviewed operation.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Returns the canonical engine selected during planning.
    #[must_use]
    pub const fn engine(&self) -> &EngineInstallation {
        &self.engine
    }

    /// Returns the preset identity used during planning.
    #[must_use]
    pub const fn preset(&self) -> &PlanPreset {
        &self.preset
    }

    /// Returns the exact backup directory reserved for this plan.
    #[must_use]
    pub fn backup_directory(&self) -> &Path {
        &self.backup_directory
    }

    /// Returns the before-and-after effective impact.
    #[must_use]
    pub const fn impact(&self) -> PlanImpact {
        self.impact
    }

    /// Returns the verified descriptor edits in stable plugin order.
    #[must_use]
    pub fn changes(&self) -> &[PlannedFileEdit] {
        &self.changes
    }

    /// Returns resolved preset actions that need no byte edit.
    #[must_use]
    pub fn no_ops(&self) -> &[PlanNoOp] {
        &self.no_ops
    }

    /// Returns requested plugins that stay enabled through dependency roots.
    #[must_use]
    pub fn dependency_warnings(&self) -> &[PlanDependencyWarning] {
        &self.dependency_warnings
    }

    /// Returns non-fatal dependency graph findings from the planned state.
    #[must_use]
    pub fn graph_warnings(&self) -> &[DependencyWarning] {
        &self.graph_warnings
    }

    /// Returns descriptor paths skipped during the read-only scan.
    #[must_use]
    pub fn scan_warnings(&self) -> &[PluginScanWarning] {
        &self.scan_warnings
    }

    /// Returns every disable pattern and its resolved plugin names.
    #[must_use]
    pub fn pattern_expansions(&self) -> &[PresetPatternExpansion] {
        &self.pattern_expansions
    }

    /// Returns exact preset rules absent from the selected engine.
    #[must_use]
    pub fn unmatched_rules(&self) -> &[UnmatchedPresetRule] {
        &self.unmatched_rules
    }
}

/// Builds one engine-mode plan without changing descriptor, preset, backup, or journal files.
///
/// # Errors
///
/// Returns an error when scanning, preset resolution, source validation, or targeted editing cannot produce an unambiguous plan.
pub fn build_engine_plan(
    engine: &EngineInstallation,
    preset_path: &Path,
    preset: &Preset,
    options: &PlanBuildOptions,
) -> Result<EnginePlan> {
    let scan = scan_engine_plugins(engine)?;
    let before_report = analyze_plugins(scan.plugins);
    let plugin_names = before_report
        .plugins
        .iter()
        .map(|plugin| plugin.name.clone())
        .collect::<Vec<_>>();
    let resolution = preset.resolve(&plugin_names)?;
    let plugin_index = index_plugins(&before_report.plugins);
    let mut after_plugins = before_report.plugins.clone();
    let mut changes = Vec::new();
    let mut no_ops = Vec::new();

    for change in &resolution.changes {
        let index = plugin_index
            .get(&normalize_name(&change.plugin))
            .copied()
            .ok_or_else(|| Error::Internal {
                message: format!(
                    "resolved preset plugin is absent from the analyzed set: {}",
                    change.plugin
                ),
            })?;
        let plugin = &before_report.plugins[index];
        if let Some(reason) = no_op_reason(change.action, plugin.declared_state) {
            no_ops.push(PlanNoOp {
                plugin: plugin.name.clone(),
                action: change.action,
                declared_state: plugin.declared_state,
                effective_enabled: plugin.effective_enabled == Some(true),
                reason,
                matched_by: change.matched_by.clone(),
            });
            continue;
        }

        let requested_state = action_state(change.action);
        changes.push(plan_file_edit(
            engine,
            plugin,
            requested_state,
            change.matched_by.clone(),
        )?);
        after_plugins[index].declared_state = requested_state;
    }

    let after_report = analyze_plugins(after_plugins);
    let impact = plan_impact(&before_report.plugins, &after_report.plugins);
    let dependency_warnings =
        requested_dependency_warnings(&resolution.changes, &after_report.plugins);
    let backup_directory = options
        .backup_root
        .join(engine_storage_id(engine))
        .join(&options.operation_id);
    verify_engine_snapshot(engine, &before_report.plugins, &scan.warnings, &changes)?;

    Ok(EnginePlan {
        schema: PLAN_SCHEMA,
        operation_id: options.operation_id.clone(),
        engine: engine.clone(),
        preset: PlanPreset {
            name: preset.name.clone(),
            path: preset_path.to_path_buf(),
        },
        backup_directory,
        impact,
        changes,
        no_ops,
        dependency_warnings,
        graph_warnings: after_report.warnings,
        scan_warnings: scan.warnings,
        pattern_expansions: resolution.pattern_expansions,
        unmatched_rules: resolution.unmatched,
    })
}

/// Returns `%APPDATA%\Unclean\backups` without creating the directory.
///
/// # Errors
///
/// Returns an error when `APPDATA` is missing or empty.
pub fn default_backup_root() -> Result<PathBuf> {
    env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("Unclean").join("backups"))
        .ok_or_else(|| Error::InvalidInput {
            message: "Unclean cannot locate APPDATA for the backup destination".to_owned(),
        })
}

/// Returns the lowercase SHA-256 digest for an exact byte stream.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(crate) fn engine_storage_id(engine: &EngineInstallation) -> String {
    let version = engine.version.as_deref().unwrap_or("unknown");
    let label = safe_path_segment(version);
    let identity = format!(
        "{}\0{}",
        normalized_path_identity(&engine.path),
        engine.version.as_deref().unwrap_or("")
    );
    let hash = sha256_hex(identity.as_bytes());
    format!("ue-{label}-{}", &hash[..12])
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
}

fn safe_path_segment(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut previous_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '.' {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !previous_separator {
            output.push('-');
            previous_separator = true;
        }
    }
    let trimmed = output.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn normalized_path_identity(path: &Path) -> String {
    let identity = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        identity.to_ascii_lowercase()
    } else {
        identity
    }
}

fn index_plugins(plugins: &[PluginDescriptor]) -> BTreeMap<String, usize> {
    plugins
        .iter()
        .enumerate()
        .map(|(index, plugin)| (normalize_name(&plugin.name), index))
        .collect()
}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn no_op_reason(
    action: PresetAction,
    declared_state: DeclaredPluginState,
) -> Option<PlanNoOpReason> {
    if action == PresetAction::Disable && declared_state == DeclaredPluginState::Unspecified {
        return Some(PlanNoOpReason::UnspecifiedAlreadyOff);
    }
    (action_state(action) == declared_state).then_some(PlanNoOpReason::AlreadyDeclared)
}

const fn action_state(action: PresetAction) -> DeclaredPluginState {
    match action {
        PresetAction::Enable => DeclaredPluginState::Enabled,
        PresetAction::Disable => DeclaredPluginState::Disabled,
        PresetAction::Clear => DeclaredPluginState::Unspecified,
    }
}

fn plan_file_edit(
    engine: &EngineInstallation,
    plugin: &PluginDescriptor,
    requested_state: DeclaredPluginState,
    matched_by: Vec<PresetRuleMatch>,
) -> Result<PlannedFileEdit> {
    let source_bytes =
        fs::read(&plugin.path).map_err(|error| plan_read_error(&plugin.path, &error))?;
    let document = DescriptorDocument::parse(&source_bytes).map_err(|error| Error::Conflict {
        message: format!(
            "{} no longer matches the scanned descriptor: {error}",
            plugin.path.display()
        ),
    })?;
    if document.declared_state() != plugin.declared_state {
        return Err(Error::Conflict {
            message: format!("{} changed after the plugin scan", plugin.path.display()),
        });
    }
    let planned_bytes = document
        .edit_enabled_by_default(requested_state)
        .map_err(|error| Error::InvalidInput {
            message: format!("Plugin edit failed at {}: {error}", plugin.path.display()),
        })?;
    if planned_bytes == source_bytes {
        return Err(Error::Internal {
            message: format!(
                "planned descriptor edit produced no byte change for {}",
                plugin.name
            ),
        });
    }
    let plugin_root = engine.path.join("Engine").join("Plugins");
    let relative_plugin_path =
        plugin
            .path
            .strip_prefix(&plugin_root)
            .map_err(|_| Error::Conflict {
                message: format!(
                    "descriptor is outside the selected engine plugin root: {}",
                    plugin.path.display()
                ),
            })?;
    let byte_change = differing_ranges(&source_bytes, &planned_bytes);

    Ok(PlannedFileEdit {
        plugin: plugin.name.clone(),
        path: plugin.path.clone(),
        relative_path: PathBuf::from("Engine")
            .join("Plugins")
            .join(relative_plugin_path),
        field: PlanField::EnabledByDefault,
        value_before: plugin.declared_state,
        value_after: requested_state,
        sha256_before: sha256_hex(&source_bytes),
        sha256_after: sha256_hex(&planned_bytes),
        planned_byte_count: planned_bytes.len(),
        byte_change,
        matched_by,
        planned_bytes,
    })
}

fn plan_read_error(path: &Path, error: &std::io::Error) -> Error {
    match error.kind() {
        ErrorKind::NotFound => Error::Conflict {
            message: format!("{} disappeared after the plugin scan", path.display()),
        },
        ErrorKind::PermissionDenied => Error::PermissionDenied {
            message: format!("descriptor read failed during planning: {}", path.display()),
        },
        _ => Error::Internal {
            message: format!("descriptor read failed during planning: {error}"),
        },
    }
}

fn verify_engine_snapshot(
    engine: &EngineInstallation,
    expected_plugins: &[PluginDescriptor],
    expected_warnings: &[PluginScanWarning],
    changes: &[PlannedFileEdit],
) -> Result<()> {
    let verification_scan = scan_engine_plugins(engine)?;
    let verification_report = analyze_plugins(verification_scan.plugins);
    if verification_report.plugins != expected_plugins
        || verification_scan.warnings != expected_warnings
    {
        return Err(Error::Conflict {
            message: "the engine plugin set changed during plan construction".to_owned(),
        });
    }
    for edit in changes {
        let current_bytes =
            fs::read(&edit.path).map_err(|error| plan_read_error(&edit.path, &error))?;
        if sha256_hex(&current_bytes) != edit.sha256_before {
            return Err(Error::Conflict {
                message: format!("{} changed during plan construction", edit.path.display()),
            });
        }
    }
    Ok(())
}

pub(crate) fn differing_ranges(source: &[u8], planned: &[u8]) -> PlannedByteChange {
    let prefix = source
        .iter()
        .zip(planned)
        .take_while(|(left, right)| left == right)
        .count();
    let remaining_source = source.len().saturating_sub(prefix);
    let remaining_planned = planned.len().saturating_sub(prefix);
    let suffix = source[prefix..]
        .iter()
        .rev()
        .zip(planned[prefix..].iter().rev())
        .take(remaining_source.min(remaining_planned))
        .take_while(|(left, right)| left == right)
        .count();
    PlannedByteChange {
        source: ByteSpan {
            start: prefix,
            end: source.len().saturating_sub(suffix),
        },
        planned: ByteSpan {
            start: prefix,
            end: planned.len().saturating_sub(suffix),
        },
    }
}

fn plan_impact(before: &[PluginDescriptor], after: &[PluginDescriptor]) -> PlanImpact {
    PlanImpact {
        default_roots: CountChange {
            before: default_root_count(before),
            after: default_root_count(after),
        },
        effective_plugins: CountChange {
            before: effective_plugin_count(before),
            after: effective_plugin_count(after),
        },
        declared_modules: CountChange {
            before: effective_module_count(before),
            after: effective_module_count(after),
        },
    }
}

fn default_root_count(plugins: &[PluginDescriptor]) -> usize {
    plugins
        .iter()
        .filter(|plugin| plugin.declared_state == DeclaredPluginState::Enabled)
        .count()
}

fn effective_plugin_count(plugins: &[PluginDescriptor]) -> usize {
    plugins
        .iter()
        .filter(|plugin| plugin.effective_enabled == Some(true))
        .count()
}

fn effective_module_count(plugins: &[PluginDescriptor]) -> usize {
    plugins
        .iter()
        .filter(|plugin| plugin.effective_enabled == Some(true))
        .map(|plugin| plugin.module_count)
        .sum()
}

fn requested_dependency_warnings(
    changes: &[crate::presets::PresetChange],
    after: &[PluginDescriptor],
) -> Vec<PlanDependencyWarning> {
    let index = index_plugins(after);
    let mut warnings = Vec::new();
    for change in changes {
        if !matches!(change.action, PresetAction::Disable | PresetAction::Clear) {
            continue;
        }
        let Some(plugin_index) = index.get(&normalize_name(&change.plugin)).copied() else {
            continue;
        };
        let plugin = &after[plugin_index];
        if plugin.effective_enabled != Some(true) {
            continue;
        }
        let roots = roots_reaching_plugin(after, plugin_index);
        let root_list = roots.join(", ");
        warnings.push(PlanDependencyWarning {
            plugin: plugin.name.clone(),
            action: change.action,
            roots,
            effective_path: plugin.effective_path.clone(),
            message: format!(
                "Plugin remains effective: {} is reached from {root_list}. Disable the listed roots or revise the preset.",
                plugin.name
            ),
        });
    }
    warnings
}

fn roots_reaching_plugin(plugins: &[PluginDescriptor], target: usize) -> Vec<String> {
    let index = index_plugins(plugins);
    let edges = plugins
        .iter()
        .map(|plugin| {
            plugin
                .enabled_dependencies
                .iter()
                .filter_map(|dependency| index.get(&normalize_name(&dependency.name)).copied())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    plugins
        .iter()
        .enumerate()
        .filter(|(_, plugin)| plugin.declared_state == DeclaredPluginState::Enabled)
        .filter(|(root, _)| path_exists(*root, target, &edges))
        .map(|(_, plugin)| plugin.name.clone())
        .collect()
}

fn path_exists(root: usize, target: usize, edges: &[Vec<usize>]) -> bool {
    let mut stack = vec![root];
    let mut visited = HashSet::new();
    while let Some(current) = stack.pop() {
        if current == target {
            return true;
        }
        if !visited.insert(current) {
            continue;
        }
        if let Some(next) = edges.get(current) {
            stack.extend(next.iter().copied());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{
        PlanBuildOptions, PlanNoOpReason, build_engine_plan, differing_ranges, sha256_hex,
    };
    use crate::descriptors::{DeclaredPluginState, DescriptorDocument};
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use crate::presets::Preset;

    #[test]
    fn hash_matches_the_standard_sha256_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn differing_ranges_cover_insertion_and_removal() {
        let insertion = differing_ranges(b"abef", b"abcdef");
        assert_eq!((insertion.source.start, insertion.source.end), (2, 2));
        assert_eq!((insertion.planned.start, insertion.planned.end), (2, 4));

        let removal = differing_ranges(b"abcdef", b"abef");
        assert_eq!((removal.source.start, removal.source.end), (2, 4));
        assert_eq!((removal.planned.start, removal.planned.end), (2, 2));
    }

    #[test]
    fn operation_identifier_cannot_escape_the_backup_root() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let error = PlanBuildOptions::new(temp.path().to_path_buf(), "..".to_owned())
            .err()
            .ok_or("unsafe operation identifier was accepted")?;

        assert!(error.to_string().contains("operation identifier"));
        Ok(())
    }

    #[test]
    fn plan_keeps_source_bytes_and_reports_effective_impact() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_Invented");
        write_plugin(
            &engine_path,
            "Runtime/A/A.uplugin",
            r#"{"EnabledByDefault":true,"Modules":[{"Name":"A"}],"Plugins":[{"Name":"B","Enabled":true}]}"#,
        )?;
        write_plugin(
            &engine_path,
            "Runtime/B/B.uplugin",
            r#"{"Modules":[{"Name":"B1"},{"Name":"B2"}]}"#,
        )?;
        write_plugin(
            &engine_path,
            "Runtime/C/C.uplugin",
            r#"{"EnabledByDefault":true,"Modules":[{"Name":"C1"},{"Name":"C2"},{"Name":"C3"}]}"#,
        )?;
        write_plugin(
            &engine_path,
            "Runtime/D/D.uplugin",
            r#"{"EnabledByDefault":false,"Modules":[{"Name":"D1"},{"Name":"D2"},{"Name":"D3"},{"Name":"D4"}]}"#,
        )?;
        let engine = engine(&engine_path)?;
        let preset = Preset {
            schema: 1,
            name: "Invented plan".to_owned(),
            description: None,
            enable: vec!["D".to_owned()],
            disable: vec!["B".to_owned(), "C".to_owned()],
            clear: Vec::new(),
            disable_matching: Vec::new(),
        };
        let options =
            PlanBuildOptions::new(temp.path().join("backups"), "test-operation".to_owned())?;
        let c_path = engine_path
            .join("Engine")
            .join("Plugins")
            .join("Runtime/C/C.uplugin");
        let c_before = fs::read(&c_path)?;

        let plan = build_engine_plan(&engine, Path::new("invented.toml"), &preset, &options)?;

        assert_eq!(plan.changes().len(), 2);
        assert_eq!(plan.no_ops().len(), 1);
        assert_eq!(
            plan.no_ops()[0].reason,
            PlanNoOpReason::UnspecifiedAlreadyOff
        );
        assert_eq!(plan.dependency_warnings().len(), 1);
        assert_eq!(plan.dependency_warnings()[0].plugin, "B");
        assert_eq!(plan.dependency_warnings()[0].roots, ["A"]);
        assert_eq!(plan.impact().default_roots.before, 2);
        assert_eq!(plan.impact().default_roots.after, 2);
        assert_eq!(plan.impact().effective_plugins.before, 3);
        assert_eq!(plan.impact().effective_plugins.after, 3);
        assert_eq!(plan.impact().declared_modules.before, 6);
        assert_eq!(plan.impact().declared_modules.after, 7);
        assert_eq!(fs::read(&c_path)?, c_before);
        let c_edit = plan
            .changes()
            .iter()
            .find(|edit| edit.plugin == "C")
            .ok_or("C edit is missing")?;
        let parsed = DescriptorDocument::parse(c_edit.planned_bytes())?;
        assert_eq!(parsed.declared_state(), DeclaredPluginState::Disabled);
        assert_eq!(c_edit.sha256_after, sha256_hex(c_edit.planned_bytes()));
        assert_eq!(c_edit.matched_by[0].rule, "C");
        Ok(())
    }

    fn write_plugin(engine: &Path, relative: &str, source: &str) -> Result<(), Box<dyn StdError>> {
        let path = engine.join("Engine").join("Plugins").join(relative);
        let parent = path.parent().ok_or("plugin fixture has no parent")?;
        fs::create_dir_all(parent)?;
        fs::write(path, source)?;
        Ok(())
    }

    fn engine(path: &Path) -> Result<EngineInstallation, Box<dyn StdError>> {
        let build = path.join("Engine").join("Build");
        fs::create_dir_all(&build)?;
        fs::write(
            build.join("Build.version"),
            r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
        )?;
        Ok(EngineInstallation {
            path: path.to_path_buf(),
            version: Some("5.9.0".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Partial,
            descriptor_count: 4,
            issues: Vec::new(),
        })
    }
}
