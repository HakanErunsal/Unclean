//! Executes reviewed plans through backup, replacement, verification, rollback, and recovery.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backups::{
    BACKUP_MANIFEST_SCHEMA, BackupManifest, BackupManifestFile, BackupOperationKind,
    load_backup_manifest,
};
use crate::descriptors::{DeclaredPluginState, DescriptorDocument};
use crate::discovery::EngineInstallation;
use crate::elevation::{
    RevalidatedEngineOperation, validate_engine_target_path, validate_project_target_path,
    validate_template_target_path,
};
use crate::journal::{
    JournalFile, JournalOperation, JournalOperationKind, JournalState, OperationTargetKind,
    load_journal, paths_match, validate_relative_plugin_path, validate_relative_target_path,
};
use crate::plans::{EnginePlan, PlanBuildOptions, sha256_hex};
use crate::platform::{copy_file, install_file, replace_file, set_readonly};
use crate::project_plans::ProjectPlan;
use crate::projects::ProjectDescriptorDocument;
use crate::templates::TemplatePlan;
use crate::{Error, Result};

/// Identifies the confirmation step required before a reviewed write may continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteConfirmation {
    /// Reports that `--yes` confirmed the reviewed plan.
    Confirmed,
    /// Reports that an interactive frontend must ask before writing.
    PromptRequired,
}

/// Reports one completed transactional operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OperationReport {
    /// Identifies the operation.
    pub operation_id: String,
    /// Identifies whether the operation applied a preset or restored a snapshot.
    pub kind: JournalOperationKind,
    /// Identifies the descriptor boundary used by the operation.
    pub target_kind: OperationTargetKind,
    /// Records the selected engine.
    pub engine: EngineInstallation,
    /// Records the selected project descriptor for project operations.
    pub project_path: Option<PathBuf>,
    /// Names the associated preset.
    pub preset: String,
    /// Records the recovery snapshot created before writing.
    pub backup_directory: Option<PathBuf>,
    /// Records the journal updated after verification.
    pub journal_path: Option<PathBuf>,
    /// Counts descriptor files replaced by the operation.
    pub files_written: usize,
    /// Reports whether the transaction committed a journal entry.
    pub recorded: bool,
}

/// Stores one reviewed restore edit and its verified recovery bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RestoreFileEdit {
    /// Records the target path relative to the engine root.
    pub relative_path: PathBuf,
    /// Records the current state when the target exists and parses.
    pub value_before: Option<DeclaredPluginState>,
    /// Records the state recovered from the selected snapshot.
    pub value_after: DeclaredPluginState,
    /// Records the current hash when the target exists.
    pub sha256_before: Option<String>,
    /// Records the verified snapshot hash.
    pub sha256_after: String,
    /// Reports the restored output size.
    pub planned_byte_count: usize,
    #[serde(skip)]
    planned_bytes: Vec<u8>,
}

/// Holds a read-only engine restore plan for review before writing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RestorePlan {
    schema: u8,
    operation_id: String,
    source_snapshot: String,
    engine: EngineInstallation,
    preset: String,
    preset_path: PathBuf,
    backup_directory: PathBuf,
    changes: Vec<RestoreFileEdit>,
}

/// Stores one reviewed project restore edit and its verified recovery bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectRestoreFileEdit {
    /// Records the canonical project descriptor path.
    pub path: PathBuf,
    /// Records the project file name relative to its parent.
    pub relative_path: PathBuf,
    /// Records the current hash.
    pub sha256_before: String,
    /// Records the verified snapshot hash.
    pub sha256_after: String,
    /// Reports the restored output size.
    pub planned_byte_count: usize,
    #[serde(skip)]
    planned_bytes: Vec<u8>,
}

impl ProjectRestoreFileEdit {
    /// Returns the verified project bytes retained by the immutable restore plan.
    #[must_use]
    pub fn planned_bytes(&self) -> &[u8] {
        &self.planned_bytes
    }
}

/// Holds a read-only project restore plan for review before writing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectRestorePlan {
    schema: u8,
    operation_id: String,
    source_snapshot: String,
    project_path: PathBuf,
    engine: EngineInstallation,
    preset: String,
    preset_path: Option<PathBuf>,
    backup_directory: PathBuf,
    change: Option<ProjectRestoreFileEdit>,
}

/// Stores one reviewed template restore edit and its verified recovery bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TemplateRestoreFileEdit {
    /// Records the target path relative to the engine root.
    pub relative_path: PathBuf,
    /// Records the current hash when the template exists.
    pub sha256_before: Option<String>,
    /// Records the verified snapshot hash.
    pub sha256_after: String,
    /// Reports the restored output size.
    pub planned_byte_count: usize,
    #[serde(skip)]
    planned_bytes: Vec<u8>,
}

impl TemplateRestoreFileEdit {
    /// Returns the verified template bytes retained by the immutable restore plan.
    #[must_use]
    pub fn planned_bytes(&self) -> &[u8] {
        &self.planned_bytes
    }
}

/// Holds a read-only template restore plan for review before writing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TemplateRestorePlan {
    schema: u8,
    operation_id: String,
    source_snapshot: String,
    engine: EngineInstallation,
    preset: String,
    backup_directory: PathBuf,
    changes: Vec<TemplateRestoreFileEdit>,
}

impl TemplateRestorePlan {
    /// Returns the machine schema for this restore plan.
    #[must_use]
    pub const fn schema(&self) -> u8 {
        self.schema
    }

    /// Returns the new operation identifier reserved for this restore.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Returns the journaled snapshot selected for recovery.
    #[must_use]
    pub fn source_snapshot(&self) -> &str {
        &self.source_snapshot
    }

    /// Returns the selected engine.
    #[must_use]
    pub const fn engine(&self) -> &EngineInstallation {
        &self.engine
    }

    /// Returns the operation label recorded by the source snapshot.
    #[must_use]
    pub fn preset(&self) -> &str {
        &self.preset
    }

    /// Returns the backup directory reserved for the restore transaction.
    #[must_use]
    pub fn backup_directory(&self) -> &Path {
        &self.backup_directory
    }

    /// Returns every template descriptor that differs from the snapshot.
    #[must_use]
    pub fn changes(&self) -> &[TemplateRestoreFileEdit] {
        &self.changes
    }
}

impl ProjectRestorePlan {
    /// Returns the machine schema for this restore plan.
    #[must_use]
    pub const fn schema(&self) -> u8 {
        self.schema
    }

    /// Returns the unique identifier reserved for this restore.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Returns the selected recovery snapshot.
    #[must_use]
    pub fn source_snapshot(&self) -> &str {
        &self.source_snapshot
    }

    /// Returns the selected project descriptor.
    #[must_use]
    pub fn project_path(&self) -> &Path {
        &self.project_path
    }

    /// Returns the engine associated with the selected project.
    #[must_use]
    pub const fn engine(&self) -> &EngineInstallation {
        &self.engine
    }

    /// Returns the source name retained from the snapshot.
    #[must_use]
    pub fn preset(&self) -> &str {
        &self.preset
    }

    /// Returns the source preset path when the snapshot recorded one.
    #[must_use]
    pub fn preset_path(&self) -> Option<&Path> {
        self.preset_path.as_deref()
    }

    /// Returns the backup directory reserved for the restore transaction.
    #[must_use]
    pub fn backup_directory(&self) -> &Path {
        &self.backup_directory
    }

    /// Returns the verified restore edit when current bytes differ from the snapshot.
    #[must_use]
    pub const fn change(&self) -> Option<&ProjectRestoreFileEdit> {
        self.change.as_ref()
    }
}

impl RestorePlan {
    /// Returns the machine schema for this restore plan.
    #[must_use]
    pub const fn schema(&self) -> u8 {
        self.schema
    }

    /// Returns the unique identifier reserved for this restore.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Returns the selected recovery snapshot.
    #[must_use]
    pub fn source_snapshot(&self) -> &str {
        &self.source_snapshot
    }

    /// Returns the selected engine.
    #[must_use]
    pub const fn engine(&self) -> &EngineInstallation {
        &self.engine
    }

    /// Returns the preset name retained from the source operation.
    #[must_use]
    pub fn preset(&self) -> &str {
        &self.preset
    }

    /// Returns the preset path retained from the source snapshot.
    #[must_use]
    pub fn preset_path(&self) -> &Path {
        &self.preset_path
    }

    /// Returns the backup directory reserved for the restore transaction.
    #[must_use]
    pub fn backup_directory(&self) -> &Path {
        &self.backup_directory
    }

    /// Returns verified snapshot edits in stable path order.
    #[must_use]
    pub fn changes(&self) -> &[RestoreFileEdit] {
        &self.changes
    }
}

/// Applies the shared confirmation rule without performing a write.
///
/// # Errors
///
/// Returns an error when a noninteractive session does not supply `--yes`.
pub fn write_confirmation(stdin_is_terminal: bool, yes: bool) -> Result<WriteConfirmation> {
    if yes {
        Ok(WriteConfirmation::Confirmed)
    } else if stdin_is_terminal {
        Ok(WriteConfirmation::PromptRequired)
    } else {
        Err(Error::InvalidInput {
            message: "review the plan, then pass --yes for a noninteractive write".to_owned(),
        })
    }
}

/// Applies one immutable engine plan with backup, verification, rollback, and journaling.
///
/// # Errors
///
/// Returns an error when source bytes changed, a write boundary fails, or verification fails.
pub fn apply_engine_plan(plan: &EnginePlan, journal_path: &Path) -> Result<OperationReport> {
    let transaction = Transaction::from_engine_plan(plan);
    execute_transaction(&transaction, journal_path, &NoFault)
}

/// Applies one immutable project plan with backup, verification, rollback, and journaling.
///
/// # Errors
///
/// Returns an error when source bytes changed, a write boundary fails, or verification fails.
pub fn apply_project_plan(plan: &ProjectPlan, journal_path: &Path) -> Result<OperationReport> {
    let transaction = Transaction::from_project_plan(plan);
    execute_transaction(&transaction, journal_path, &NoFault)
}

/// Applies one immutable template plan with backup, verification, rollback, and journaling.
///
/// # Errors
///
/// Returns an error when source bytes changed, a write boundary fails, or verification fails.
pub fn apply_template_plan(plan: &TemplatePlan, journal_path: &Path) -> Result<OperationReport> {
    let transaction = Transaction::from_template_plan(plan);
    execute_transaction(&transaction, journal_path, &NoFault)
}

/// Applies one worker-rederived operation through the protected transaction writer.
///
/// # Errors
///
/// Returns an error when source bytes change or a transaction boundary fails.
pub(crate) fn apply_revalidated_operation(
    operation: &RevalidatedEngineOperation,
    journal_path: &Path,
) -> Result<OperationReport> {
    let transaction = Transaction::from_revalidated(operation);
    execute_transaction(&transaction, journal_path, &NoFault)
}

/// Applies engine plans in order and stops at the first failed engine transaction.
///
/// Completed engines remain committed when a later engine fails.
///
/// # Errors
///
/// Returns the first engine transaction failure.
pub fn apply_engine_plans(
    plans: &[EnginePlan],
    journal_path: &Path,
) -> Result<Vec<OperationReport>> {
    let mut reports = Vec::with_capacity(plans.len());
    for plan in plans {
        reports.push(apply_engine_plan(plan, journal_path)?);
    }
    Ok(reports)
}

/// Builds a restore plan from one completed operation and its verified backup manifest.
///
/// # Errors
///
/// Returns an error when the snapshot, manifest, or recovery bytes are missing or invalid.
pub fn build_restore_plan(
    engine: &EngineInstallation,
    snapshot: &str,
    journal_path: &Path,
    options: &PlanBuildOptions,
) -> Result<RestorePlan> {
    let state = load_journal(journal_path)?;
    let operation = state
        .operations
        .iter()
        .find(|operation| {
            operation.id == snapshot
                && operation.target_kind == OperationTargetKind::Engine
                && paths_match(&operation.engine_path, &engine.path)
        })
        .ok_or_else(|| Error::NotFound {
            item: format!("snapshot \"{snapshot}\" for {}", engine.path.display()),
        })?;
    let manifest = load_backup_manifest(&operation.backup_directory)?;
    let expected_kind = match operation.kind {
        JournalOperationKind::Apply => BackupOperationKind::Apply,
        JournalOperationKind::Restore => BackupOperationKind::Restore,
    };
    if manifest.operation_id != operation.id
        || manifest.operation != expected_kind
        || manifest.target_kind != OperationTargetKind::Engine
        || !paths_match(&manifest.engine_path, &engine.path)
        || manifest.project_path.is_some()
        || manifest.preset != operation.preset
        || manifest.files.len() != operation.files.len()
        || !manifest
            .files
            .iter()
            .zip(&operation.files)
            .all(|(manifest_file, journal_file)| {
                manifest_file.relative_path == journal_file.relative_path
                    && manifest_file.sha256_after == journal_file.sha256_after
            })
    {
        return Err(Error::Conflict {
            message: format!(
                "snapshot manifest does not match journal operation {}",
                operation.id
            ),
        });
    }

    let mut changes = manifest
        .files
        .iter()
        .map(|file| restore_file_edit(engine, operation, file))
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>>>()?;
    changes.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let preset_path = manifest.preset_path.ok_or_else(|| Error::Conflict {
        message: format!(
            "snapshot manifest has no preset path for engine operation {}",
            operation.id
        ),
    })?;
    Ok(RestorePlan {
        schema: 1,
        operation_id: options.operation_id().to_owned(),
        source_snapshot: operation.id.clone(),
        engine: engine.clone(),
        preset: operation.preset.clone(),
        preset_path,
        backup_directory: options.backup_directory(engine),
        changes,
    })
}

/// Builds a project restore plan from one completed project operation.
///
/// # Errors
///
/// Returns an error when the project, journal entry, manifest, or recovery bytes do not match.
pub fn build_project_restore_plan(
    project_path: &Path,
    engines: &[EngineInstallation],
    snapshot: &str,
    journal_path: &Path,
    options: &PlanBuildOptions,
) -> Result<ProjectRestorePlan> {
    let project_path = fs::canonicalize(project_path).map_err(|error| Error::NotFound {
        item: format!("project descriptor {}: {error}", project_path.display()),
    })?;
    let current_bytes = read_file(&project_path, "current project descriptor")?;
    let document =
        ProjectDescriptorDocument::parse(&current_bytes).map_err(|error| Error::InvalidInput {
            message: error.to_string(),
        })?;
    let engine = document
        .resolve_associated_engine(engines)
        .map_err(|error| Error::NotFound {
            item: error.to_string(),
        })?
        .clone();
    let state = load_journal(journal_path)?;
    let operation = project_snapshot_operation(&state, snapshot, &project_path)?;
    let manifest = load_backup_manifest(&operation.backup_directory)?;
    let file = validate_project_snapshot_manifest(operation, &manifest, &engine, &project_path)?;
    let change = project_restore_edit(&project_path, &current_bytes, operation, file)?;
    Ok(ProjectRestorePlan {
        schema: 1,
        operation_id: options.operation_id().to_owned(),
        source_snapshot: operation.id.clone(),
        backup_directory: options.project_backup_directory(&project_path),
        project_path,
        engine,
        preset: operation.preset.clone(),
        preset_path: manifest.preset_path,
        change,
    })
}

/// Builds a template restore plan from one completed template operation.
///
/// # Errors
///
/// Returns an error when the journal entry, manifest, or recovery bytes do not match.
pub fn build_template_restore_plan(
    engine: &EngineInstallation,
    snapshot: &str,
    journal_path: &Path,
    options: &PlanBuildOptions,
) -> Result<TemplateRestorePlan> {
    let state = load_journal(journal_path)?;
    let operation = state
        .operations
        .iter()
        .find(|operation| {
            operation.id == snapshot
                && operation.target_kind == OperationTargetKind::Template
                && paths_match(&operation.engine_path, &engine.path)
        })
        .ok_or_else(|| Error::NotFound {
            item: format!(
                "template snapshot \"{snapshot}\" for {}",
                engine.path.display()
            ),
        })?;
    let manifest = load_backup_manifest(&operation.backup_directory)?;
    validate_template_snapshot_manifest(operation, &manifest, engine)?;
    let mut changes = manifest
        .files
        .iter()
        .map(|file| template_restore_edit(engine, operation, file))
        .filter_map(Result::transpose)
        .collect::<Result<Vec<_>>>()?;
    changes.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(TemplateRestorePlan {
        schema: 1,
        operation_id: options.operation_id().to_owned(),
        source_snapshot: operation.id.clone(),
        engine: engine.clone(),
        preset: operation.preset.clone(),
        backup_directory: options.backup_directory(engine),
        changes,
    })
}

fn validate_template_snapshot_manifest(
    operation: &JournalOperation,
    manifest: &BackupManifest,
    engine: &EngineInstallation,
) -> Result<()> {
    let expected_kind = match operation.kind {
        JournalOperationKind::Apply => BackupOperationKind::Apply,
        JournalOperationKind::Restore => BackupOperationKind::Restore,
    };
    if manifest.operation_id != operation.id
        || manifest.operation != expected_kind
        || manifest.target_kind != OperationTargetKind::Template
        || !paths_match(&manifest.engine_path, &engine.path)
        || manifest.project_path.is_some()
        || manifest.preset != operation.preset
        || manifest.files.len() != operation.files.len()
        || !manifest
            .files
            .iter()
            .zip(&operation.files)
            .all(|(manifest_file, journal_file)| {
                manifest_file.relative_path == journal_file.relative_path
                    && manifest_file.sha256_after == journal_file.sha256_after
            })
    {
        return Err(Error::Conflict {
            message: format!(
                "template snapshot manifest does not match journal operation {}",
                operation.id
            ),
        });
    }
    for file in &manifest.files {
        validate_relative_target_path(OperationTargetKind::Template, &file.relative_path)?;
        if !file.source_existed || file.value_before.is_some() || file.value_after.is_some() {
            return Err(Error::Conflict {
                message: format!(
                    "template snapshot metadata is invalid for {}",
                    file.relative_path.display()
                ),
            });
        }
    }
    Ok(())
}

fn template_restore_edit(
    engine: &EngineInstallation,
    operation: &JournalOperation,
    file: &BackupManifestFile,
) -> Result<Option<TemplateRestoreFileEdit>> {
    let planned_bytes = read_file(
        &operation.backup_directory.join(&file.relative_path),
        "snapshot template descriptor",
    )?;
    let planned_hash = sha256_hex(&planned_bytes);
    if file.sha256_before.as_deref() != Some(planned_hash.as_str()) {
        return Err(Error::Conflict {
            message: format!(
                "template snapshot bytes do not match the manifest for {}",
                file.relative_path.display()
            ),
        });
    }
    ProjectDescriptorDocument::parse(&planned_bytes).map_err(|error| Error::InvalidInput {
        message: format!(
            "Snapshot template descriptor is invalid for {}: {error}. Repair the snapshot before restoring.",
            file.relative_path.display()
        ),
    })?;
    let target = engine.path.join(&file.relative_path);
    let current = match fs::read(&target) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(read_error("current template descriptor", &target, &error)),
    };
    let current_hash = current.as_deref().map(sha256_hex);
    if current_hash.as_deref() == Some(planned_hash.as_str()) {
        return Ok(None);
    }
    if let Some(bytes) = &current {
        ProjectDescriptorDocument::parse(bytes).map_err(|error| Error::InvalidInput {
            message: format!(
                "Current template descriptor is invalid for {}: {error}. Repair the descriptor before restoring.",
                file.relative_path.display()
            ),
        })?;
    }
    Ok(Some(TemplateRestoreFileEdit {
        relative_path: file.relative_path.clone(),
        sha256_before: current_hash,
        sha256_after: planned_hash,
        planned_byte_count: planned_bytes.len(),
        planned_bytes,
    }))
}

fn project_snapshot_operation<'a>(
    state: &'a JournalState,
    snapshot: &str,
    project_path: &Path,
) -> Result<&'a JournalOperation> {
    state
        .operations
        .iter()
        .find(|operation| {
            operation.id == snapshot
                && operation.target_kind == OperationTargetKind::Project
                && operation
                    .project_path
                    .as_ref()
                    .is_some_and(|path| paths_match(path, project_path))
        })
        .ok_or_else(|| Error::NotFound {
            item: format!(
                "project snapshot \"{snapshot}\" for {}",
                project_path.display()
            ),
        })
}

fn validate_project_snapshot_manifest<'a>(
    operation: &JournalOperation,
    manifest: &'a BackupManifest,
    engine: &EngineInstallation,
    project_path: &Path,
) -> Result<&'a BackupManifestFile> {
    let expected_kind = match operation.kind {
        JournalOperationKind::Apply => BackupOperationKind::Apply,
        JournalOperationKind::Restore => BackupOperationKind::Restore,
    };
    if manifest.operation_id != operation.id
        || manifest.operation != expected_kind
        || manifest.target_kind != OperationTargetKind::Project
        || !paths_match(&manifest.engine_path, &engine.path)
        || manifest
            .project_path
            .as_ref()
            .is_none_or(|path| !paths_match(path, project_path))
        || manifest.preset != operation.preset
        || manifest.files.len() != 1
        || manifest.files.len() != operation.files.len()
        || !manifest
            .files
            .iter()
            .zip(&operation.files)
            .all(|(manifest_file, journal_file)| {
                manifest_file.relative_path == journal_file.relative_path
                    && manifest_file.sha256_after == journal_file.sha256_after
            })
    {
        return Err(Error::Conflict {
            message: format!(
                "project snapshot manifest does not match journal operation {}",
                operation.id
            ),
        });
    }
    let file = &manifest.files[0];
    validate_relative_target_path(OperationTargetKind::Project, &file.relative_path)?;
    if !file.source_existed
        || file.value_before.is_some()
        || file.value_after.is_some()
        || project_path.file_name() != file.relative_path.file_name()
    {
        return Err(Error::Conflict {
            message: format!(
                "project snapshot metadata is invalid for {}",
                file.relative_path.display()
            ),
        });
    }
    Ok(file)
}

fn project_restore_edit(
    project_path: &Path,
    current_bytes: &[u8],
    operation: &JournalOperation,
    file: &BackupManifestFile,
) -> Result<Option<ProjectRestoreFileEdit>> {
    let planned_bytes = read_file(
        &operation.backup_directory.join(&file.relative_path),
        "snapshot project descriptor",
    )?;
    let planned_hash = sha256_hex(&planned_bytes);
    if file.sha256_before.as_deref() != Some(planned_hash.as_str()) {
        return Err(Error::Conflict {
            message: format!(
                "project snapshot bytes do not match the manifest for {}",
                file.relative_path.display()
            ),
        });
    }
    ProjectDescriptorDocument::parse(&planned_bytes).map_err(|error| Error::InvalidInput {
        message: format!(
            "snapshot project descriptor is invalid for {}: {error}",
            file.relative_path.display()
        ),
    })?;
    let current_hash = sha256_hex(current_bytes);
    let change = (current_hash != planned_hash).then(|| ProjectRestoreFileEdit {
        path: project_path.to_path_buf(),
        relative_path: file.relative_path.clone(),
        sha256_before: current_hash,
        sha256_after: planned_hash,
        planned_byte_count: planned_bytes.len(),
        planned_bytes,
    });
    Ok(change)
}

fn restore_file_edit(
    engine: &EngineInstallation,
    operation: &JournalOperation,
    file: &BackupManifestFile,
) -> Result<Option<RestoreFileEdit>> {
    if !file.source_existed {
        return Err(Error::InvalidInput {
            message: format!(
                "snapshot {} records an absent source for {}; this build cannot restore deletion snapshots",
                operation.id,
                file.relative_path.display()
            ),
        });
    }
    validate_relative_plugin_path(&file.relative_path)?;
    let backup_path = operation.backup_directory.join(&file.relative_path);
    let planned_bytes = read_file(&backup_path, "snapshot descriptor")?;
    let planned_hash = sha256_hex(&planned_bytes);
    if file.sha256_before.as_deref() != Some(planned_hash.as_str()) {
        return Err(Error::Conflict {
            message: format!(
                "snapshot bytes do not match the manifest for {}",
                file.relative_path.display()
            ),
        });
    }
    let value_after = DescriptorDocument::parse(&planned_bytes)
        .map_err(|error| Error::InvalidInput {
            message: format!(
                "snapshot descriptor is invalid for {}: {error}",
                file.relative_path.display()
            ),
        })?
        .declared_state();
    if file.value_before != Some(value_after) || file.value_after.is_none() {
        return Err(Error::Conflict {
            message: format!(
                "snapshot state does not match the manifest for {}",
                file.relative_path.display()
            ),
        });
    }
    let target = engine.path.join(&file.relative_path);
    let current = match fs::read(&target) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(read_error("current descriptor", &target, &error)),
    };
    let current_hash = current.as_deref().map(sha256_hex);
    if current_hash.as_deref() == Some(planned_hash.as_str()) {
        return Ok(None);
    }
    let value_before = current
        .as_deref()
        .map(DescriptorDocument::parse)
        .transpose()
        .map_err(|error| Error::InvalidInput {
            message: format!(
                "current descriptor is invalid for {}: {error}",
                file.relative_path.display()
            ),
        })?
        .map(|document| document.declared_state());
    Ok(Some(RestoreFileEdit {
        relative_path: file.relative_path.clone(),
        value_before,
        value_after,
        sha256_before: current_hash,
        sha256_after: planned_hash,
        planned_byte_count: planned_bytes.len(),
        planned_bytes,
    }))
}

/// Restores one reviewed snapshot plan through the same protected writer used by apply.
///
/// # Errors
///
/// Returns an error when source bytes changed, a write boundary fails, or verification fails.
pub fn restore_engine_plan(plan: &RestorePlan, journal_path: &Path) -> Result<OperationReport> {
    let transaction = Transaction::from_restore_plan(plan);
    execute_transaction(&transaction, journal_path, &NoFault)
}

/// Restores one reviewed project snapshot through the protected transaction writer.
///
/// # Errors
///
/// Returns an error when source bytes changed, a write boundary fails, or verification fails.
pub fn restore_project_plan(
    plan: &ProjectRestorePlan,
    journal_path: &Path,
) -> Result<OperationReport> {
    let transaction = Transaction::from_project_restore_plan(plan);
    execute_transaction(&transaction, journal_path, &NoFault)
}

/// Restores one reviewed template snapshot through the protected transaction writer.
///
/// # Errors
///
/// Returns an error when source bytes changed, a write boundary fails, or verification fails.
pub fn restore_template_plan(
    plan: &TemplateRestorePlan,
    journal_path: &Path,
) -> Result<OperationReport> {
    let transaction = Transaction::from_template_restore_plan(plan);
    execute_transaction(&transaction, journal_path, &NoFault)
}

#[derive(Clone)]
struct Transaction {
    operation_id: String,
    kind: JournalOperationKind,
    target_kind: OperationTargetKind,
    engine: EngineInstallation,
    project_path: Option<PathBuf>,
    preset: String,
    preset_path: Option<PathBuf>,
    source_snapshot: Option<String>,
    backup_directory: PathBuf,
    files: Vec<TransactionFile>,
}

impl Transaction {
    fn from_engine_plan(plan: &EnginePlan) -> Self {
        Self {
            operation_id: plan.operation_id().to_owned(),
            kind: JournalOperationKind::Apply,
            target_kind: OperationTargetKind::Engine,
            engine: plan.engine().clone(),
            project_path: None,
            preset: plan.preset().name.clone(),
            preset_path: Some(plan.preset().path.clone()),
            source_snapshot: None,
            backup_directory: plan.backup_directory().to_path_buf(),
            files: plan
                .changes()
                .iter()
                .map(|edit| TransactionFile {
                    target_kind: OperationTargetKind::Engine,
                    target: edit.path.clone(),
                    relative_path: edit.relative_path.clone(),
                    expected_source_hash: Some(edit.sha256_before.clone()),
                    planned_hash: edit.sha256_after.clone(),
                    value_before: Some(edit.value_before),
                    value_after: Some(edit.value_after),
                    planned_bytes: edit.planned_bytes().to_vec(),
                })
                .collect(),
        }
    }

    fn from_project_plan(plan: &ProjectPlan) -> Self {
        Self {
            operation_id: plan.operation_id().to_owned(),
            kind: JournalOperationKind::Apply,
            target_kind: OperationTargetKind::Project,
            engine: plan.engine().clone(),
            project_path: Some(plan.project_path().to_path_buf()),
            preset: plan.source().name.clone(),
            preset_path: plan.source().path.clone(),
            source_snapshot: None,
            backup_directory: plan.backup_directory().to_path_buf(),
            files: plan
                .change()
                .map(|edit| TransactionFile {
                    target_kind: OperationTargetKind::Project,
                    target: edit.path.clone(),
                    relative_path: edit.relative_path.clone(),
                    expected_source_hash: Some(edit.sha256_before.clone()),
                    planned_hash: edit.sha256_after.clone(),
                    value_before: None,
                    value_after: None,
                    planned_bytes: edit.planned_bytes().to_vec(),
                })
                .into_iter()
                .collect(),
        }
    }

    fn from_template_plan(plan: &TemplatePlan) -> Self {
        Self {
            operation_id: plan.operation_id().to_owned(),
            kind: JournalOperationKind::Apply,
            target_kind: OperationTargetKind::Template,
            engine: plan.engine().clone(),
            project_path: None,
            preset: "Template suppression".to_owned(),
            preset_path: None,
            source_snapshot: None,
            backup_directory: plan.backup_directory().to_path_buf(),
            files: plan
                .changes()
                .iter()
                .map(|edit| TransactionFile {
                    target_kind: OperationTargetKind::Template,
                    target: edit.path.clone(),
                    relative_path: edit.relative_path.clone(),
                    expected_source_hash: Some(edit.sha256_before.clone()),
                    planned_hash: edit.sha256_after.clone(),
                    value_before: None,
                    value_after: None,
                    planned_bytes: edit.planned_bytes().to_vec(),
                })
                .collect(),
        }
    }

    fn from_restore_plan(plan: &RestorePlan) -> Self {
        Self {
            operation_id: plan.operation_id.clone(),
            kind: JournalOperationKind::Restore,
            target_kind: OperationTargetKind::Engine,
            engine: plan.engine.clone(),
            project_path: None,
            preset: plan.preset.clone(),
            preset_path: Some(plan.preset_path.clone()),
            source_snapshot: Some(plan.source_snapshot.clone()),
            backup_directory: plan.backup_directory.clone(),
            files: plan
                .changes
                .iter()
                .map(|edit| TransactionFile {
                    target_kind: OperationTargetKind::Engine,
                    target: plan.engine.path.join(&edit.relative_path),
                    relative_path: edit.relative_path.clone(),
                    expected_source_hash: edit.sha256_before.clone(),
                    planned_hash: edit.sha256_after.clone(),
                    value_before: edit.value_before,
                    value_after: Some(edit.value_after),
                    planned_bytes: edit.planned_bytes.clone(),
                })
                .collect(),
        }
    }

    fn from_project_restore_plan(plan: &ProjectRestorePlan) -> Self {
        Self {
            operation_id: plan.operation_id.clone(),
            kind: JournalOperationKind::Restore,
            target_kind: OperationTargetKind::Project,
            engine: plan.engine.clone(),
            project_path: Some(plan.project_path.clone()),
            preset: plan.preset.clone(),
            preset_path: plan.preset_path.clone(),
            source_snapshot: Some(plan.source_snapshot.clone()),
            backup_directory: plan.backup_directory.clone(),
            files: plan
                .change
                .as_ref()
                .map(|edit| TransactionFile {
                    target_kind: OperationTargetKind::Project,
                    target: edit.path.clone(),
                    relative_path: edit.relative_path.clone(),
                    expected_source_hash: Some(edit.sha256_before.clone()),
                    planned_hash: edit.sha256_after.clone(),
                    value_before: None,
                    value_after: None,
                    planned_bytes: edit.planned_bytes.clone(),
                })
                .into_iter()
                .collect(),
        }
    }

    fn from_template_restore_plan(plan: &TemplateRestorePlan) -> Self {
        Self {
            operation_id: plan.operation_id.clone(),
            kind: JournalOperationKind::Restore,
            target_kind: OperationTargetKind::Template,
            engine: plan.engine.clone(),
            project_path: None,
            preset: plan.preset.clone(),
            preset_path: None,
            source_snapshot: Some(plan.source_snapshot.clone()),
            backup_directory: plan.backup_directory.clone(),
            files: plan
                .changes
                .iter()
                .map(|edit| TransactionFile {
                    target_kind: OperationTargetKind::Template,
                    target: plan.engine.path.join(&edit.relative_path),
                    relative_path: edit.relative_path.clone(),
                    expected_source_hash: edit.sha256_before.clone(),
                    planned_hash: edit.sha256_after.clone(),
                    value_before: None,
                    value_after: None,
                    planned_bytes: edit.planned_bytes.clone(),
                })
                .collect(),
        }
    }

    fn from_revalidated(operation: &RevalidatedEngineOperation) -> Self {
        Self {
            operation_id: operation.request_id.clone(),
            kind: JournalOperationKind::Apply,
            target_kind: OperationTargetKind::Engine,
            engine: operation.engine.clone(),
            project_path: None,
            preset: operation.preset.clone(),
            preset_path: Some(operation.preset_path.clone()),
            source_snapshot: None,
            backup_directory: operation.backup_directory.clone(),
            files: operation
                .files
                .iter()
                .map(|file| TransactionFile {
                    target_kind: OperationTargetKind::Engine,
                    target: file.target.clone(),
                    relative_path: file.relative_path.clone(),
                    expected_source_hash: Some(file.source_sha256.clone()),
                    planned_hash: file.planned_sha256.clone(),
                    value_before: Some(file.value_before),
                    value_after: Some(file.value_after),
                    planned_bytes: file.planned_bytes.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
struct TransactionFile {
    target_kind: OperationTargetKind,
    target: PathBuf,
    relative_path: PathBuf,
    expected_source_hash: Option<String>,
    planned_hash: String,
    value_before: Option<DeclaredPluginState>,
    value_after: Option<DeclaredPluginState>,
    planned_bytes: Vec<u8>,
}

struct PreparedFile {
    transaction: TransactionFile,
    source_bytes: Option<Vec<u8>>,
    source_permissions: Option<Permissions>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Boundary {
    BackupWrite,
    BackupSync,
    ManifestSync,
    TemporaryShortWrite,
    TemporarySync,
    Replacement,
    Verification,
    JournalSync,
    RollbackReplacement,
}

trait FaultInjector {
    fn fails(&self, boundary: Boundary, index: usize) -> bool;
}

struct NoFault;

impl FaultInjector for NoFault {
    fn fails(&self, _boundary: Boundary, _index: usize) -> bool {
        false
    }
}

fn execute_transaction(
    transaction: &Transaction,
    journal_path: &Path,
    faults: &dyn FaultInjector,
) -> Result<OperationReport> {
    if transaction.files.is_empty() {
        return Ok(OperationReport {
            operation_id: transaction.operation_id.clone(),
            kind: transaction.kind,
            target_kind: transaction.target_kind,
            engine: transaction.engine.clone(),
            project_path: transaction.project_path.clone(),
            preset: transaction.preset.clone(),
            backup_directory: None,
            journal_path: None,
            files_written: 0,
            recorded: false,
        });
    }
    let mut journal = load_journal(journal_path)?;
    if journal
        .operations
        .iter()
        .any(|operation| operation.id == transaction.operation_id)
    {
        return Err(Error::Conflict {
            message: format!(
                "operation identifier {} is already recorded",
                transaction.operation_id
            ),
        });
    }
    let prepared = prepare_sources(transaction)?;
    prepare_backups(transaction, &prepared, faults)?;

    let mut written = Vec::new();
    for (index, file) in prepared.iter().enumerate() {
        if let Err(error) = replace_target(transaction, file, index, faults) {
            if file.source_bytes.is_some() || file.transaction.target.exists() {
                written.push(index);
            }
            return rollback_after_failure(transaction, &prepared, &written, faults, error);
        }
        written.push(index);
    }

    let completed = time_label()?;
    journal.operations.push(JournalOperation {
        id: transaction.operation_id.clone(),
        kind: transaction.kind,
        target_kind: transaction.target_kind,
        engine_path: transaction.engine.path.clone(),
        engine_version: transaction.engine.version.clone(),
        project_path: transaction.project_path.clone(),
        preset: transaction.preset.clone(),
        completed,
        backup_directory: transaction.backup_directory.clone(),
        source_snapshot: transaction.source_snapshot.clone(),
        files: transaction
            .files
            .iter()
            .map(|file| JournalFile {
                relative_path: file.relative_path.clone(),
                sha256_after: file.planned_hash.clone(),
            })
            .collect(),
    });
    if let Err(error) = write_journal(journal_path, &journal, faults) {
        return rollback_after_failure(transaction, &prepared, &written, faults, error);
    }

    Ok(OperationReport {
        operation_id: transaction.operation_id.clone(),
        kind: transaction.kind,
        target_kind: transaction.target_kind,
        engine: transaction.engine.clone(),
        project_path: transaction.project_path.clone(),
        preset: transaction.preset.clone(),
        backup_directory: Some(transaction.backup_directory.clone()),
        journal_path: Some(journal_path.to_path_buf()),
        files_written: written.len(),
        recorded: true,
    })
}

fn prepare_sources(transaction: &Transaction) -> Result<Vec<PreparedFile>> {
    let mut prepared = Vec::with_capacity(transaction.files.len());
    for file in &transaction.files {
        validate_transaction_target(transaction, file)?;
        let source_bytes = match fs::read(&file.target) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(read_error("source descriptor", &file.target, &error)),
        };
        if source_bytes.as_deref().map(sha256_hex).as_deref()
            != file.expected_source_hash.as_deref()
        {
            return Err(Error::Conflict {
                message: format!(
                    "source bytes changed after planning for {}",
                    file.relative_path.display()
                ),
            });
        }
        verify_planned_file(file)?;
        let source_permissions = match &source_bytes {
            Some(_) => Some(
                fs::metadata(&file.target)
                    .map_err(|error| read_error("source metadata", &file.target, &error))?
                    .permissions(),
            ),
            None => None,
        };
        prepared.push(PreparedFile {
            transaction: file.clone(),
            source_bytes,
            source_permissions,
        });
    }
    Ok(prepared)
}

fn validate_transaction_target(transaction: &Transaction, file: &TransactionFile) -> Result<()> {
    if file.target_kind != transaction.target_kind {
        return Err(Error::Conflict {
            message: "planned file target kind differs from the transaction boundary".to_owned(),
        });
    }
    validate_relative_target_path(transaction.target_kind, &file.relative_path)?;
    match transaction.target_kind {
        OperationTargetKind::Engine => {
            validate_engine_target_path(
                &transaction.engine.path,
                &file.relative_path,
                file.expected_source_hash.is_none(),
            )?;
            if file.target != transaction.engine.path.join(&file.relative_path) {
                return Err(Error::Conflict {
                    message: format!(
                        "planned target leaves the selected engine: {}",
                        file.target.display()
                    ),
                });
            }
        }
        OperationTargetKind::Project => {
            let project_path =
                transaction
                    .project_path
                    .as_ref()
                    .ok_or_else(|| Error::Conflict {
                        message: "project transaction has no selected project path".to_owned(),
                    })?;
            let resolved = validate_project_target_path(project_path)?;
            let parent = project_path.parent().ok_or_else(|| Error::InvalidInput {
                message: format!(
                    "selected project has no parent directory: {}",
                    project_path.display()
                ),
            })?;
            if !paths_match(&resolved, project_path)
                || !paths_match(&file.target, project_path)
                || !paths_match(&parent.join(&file.relative_path), project_path)
            {
                return Err(Error::Conflict {
                    message: format!(
                        "planned target differs from the selected project: {}",
                        file.target.display()
                    ),
                });
            }
        }
        OperationTargetKind::Template => {
            validate_template_target_path(
                &transaction.engine.path,
                &file.relative_path,
                file.expected_source_hash.is_none(),
            )?;
            if !paths_match(
                &file.target,
                &transaction.engine.path.join(&file.relative_path),
            ) {
                return Err(Error::Conflict {
                    message: format!(
                        "planned target leaves the selected template directory: {}",
                        file.target.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

fn verify_planned_file(file: &TransactionFile) -> Result<()> {
    if sha256_hex(&file.planned_bytes) != file.planned_hash {
        return Err(Error::Conflict {
            message: format!(
                "planned bytes do not match the reviewed hash for {}",
                file.relative_path.display()
            ),
        });
    }
    verify_transaction_bytes(file, &file.planned_bytes, false)?;
    Ok(())
}

fn verify_transaction_bytes(
    file: &TransactionFile,
    bytes: &[u8],
    write_boundary: bool,
) -> Result<()> {
    match file.target_kind {
        OperationTargetKind::Engine => {
            let state = DescriptorDocument::parse(bytes)
                .map_err(|error| {
                    if write_boundary {
                        Error::WriteFailed {
                            message: format!(
                                "written descriptor failed parsing for {}: {error}",
                                file.relative_path.display()
                            ),
                        }
                    } else {
                        Error::InvalidInput {
                            message: format!(
                                "planned descriptor is invalid for {}: {error}",
                                file.relative_path.display()
                            ),
                        }
                    }
                })?
                .declared_state();
            if file.value_after != Some(state) {
                return Err(if write_boundary {
                    Error::WriteFailed {
                        message: format!(
                            "written descriptor has the wrong state for {}",
                            file.relative_path.display()
                        ),
                    }
                } else {
                    Error::Conflict {
                        message: format!(
                            "planned descriptor state changed for {}",
                            file.relative_path.display()
                        ),
                    }
                });
            }
        }
        OperationTargetKind::Project => {
            if file.value_before.is_some() || file.value_after.is_some() {
                return Err(Error::Conflict {
                    message: "project transaction contains engine descriptor state".to_owned(),
                });
            }
            ProjectDescriptorDocument::parse(bytes).map_err(|error| {
                if write_boundary {
                    Error::WriteFailed {
                        message: format!(
                            "written project descriptor failed parsing for {}: {error}",
                            file.relative_path.display()
                        ),
                    }
                } else {
                    Error::InvalidInput {
                        message: format!(
                            "planned project descriptor is invalid for {}: {error}",
                            file.relative_path.display()
                        ),
                    }
                }
            })?;
        }
        OperationTargetKind::Template => {
            if file.value_before.is_some() || file.value_after.is_some() {
                return Err(Error::Conflict {
                    message: "template transaction contains engine descriptor state".to_owned(),
                });
            }
            ProjectDescriptorDocument::parse(bytes).map_err(|error| {
                if write_boundary {
                    Error::WriteFailed {
                        message: format!(
                            "written template descriptor failed parsing for {}: {error}",
                            file.relative_path.display()
                        ),
                    }
                } else {
                    Error::InvalidInput {
                        message: format!(
                            "planned template descriptor is invalid for {}: {error}",
                            file.relative_path.display()
                        ),
                    }
                }
            })?;
        }
    }
    Ok(())
}

fn prepare_backups(
    transaction: &Transaction,
    prepared: &[PreparedFile],
    faults: &dyn FaultInjector,
) -> Result<()> {
    let parent = transaction
        .backup_directory
        .parent()
        .ok_or_else(|| Error::InvalidInput {
            message: "backup directory has no parent".to_owned(),
        })?;
    fs::create_dir_all(parent)
        .map_err(|error| write_error("backup parent creation", parent, &error))?;
    fs::create_dir(&transaction.backup_directory).map_err(|error| {
        if error.kind() == ErrorKind::AlreadyExists {
            Error::Conflict {
                message: format!(
                    "backup directory already exists: {}",
                    transaction.backup_directory.display()
                ),
            }
        } else {
            write_error(
                "backup directory creation",
                &transaction.backup_directory,
                &error,
            )
        }
    })?;

    let mut manifest_files = Vec::with_capacity(prepared.len());
    for (index, file) in prepared.iter().enumerate() {
        backup_prepared_file(transaction, file, index, faults)?;
        manifest_files.push(manifest_file(file));
    }
    let manifest = BackupManifest {
        schema: BACKUP_MANIFEST_SCHEMA,
        operation_id: transaction.operation_id.clone(),
        operation: match transaction.kind {
            JournalOperationKind::Apply => BackupOperationKind::Apply,
            JournalOperationKind::Restore => BackupOperationKind::Restore,
        },
        target_kind: transaction.target_kind,
        engine_path: transaction.engine.path.clone(),
        engine_version: transaction.engine.version.clone(),
        project_path: transaction.project_path.clone(),
        created: time_label()?,
        preset: transaction.preset.clone(),
        preset_path: transaction.preset_path.clone(),
        source_snapshot: transaction.source_snapshot.clone(),
        files: manifest_files,
    };
    let manifest_path = transaction.backup_directory.join("manifest.toml");
    write_new_file(
        &manifest_path,
        manifest.render().as_bytes(),
        Boundary::BackupWrite,
        Boundary::ManifestSync,
        prepared.len(),
        faults,
    )?;
    let verified = load_backup_manifest(&transaction.backup_directory)?;
    if verified != manifest {
        return Err(Error::WriteFailed {
            message: format!(
                "backup manifest verification failed at {}; Unclean changed no target files",
                manifest_path.display()
            ),
        });
    }
    Ok(())
}

fn backup_prepared_file(
    transaction: &Transaction,
    file: &PreparedFile,
    index: usize,
    faults: &dyn FaultInjector,
) -> Result<()> {
    let Some(source) = &file.source_bytes else {
        return Ok(());
    };
    let backup_path = transaction
        .backup_directory
        .join(&file.transaction.relative_path);
    let backup_parent = backup_path.parent().ok_or_else(|| Error::Internal {
        message: format!("backup target has no parent: {}", backup_path.display()),
    })?;
    fs::create_dir_all(backup_parent)
        .map_err(|error| write_error("backup directory creation", backup_parent, &error))?;
    if faults.fails(Boundary::BackupWrite, index) {
        return Err(Error::WriteFailed {
            message: format!(
                "backup copy failed for {} by injected fault",
                backup_path.display()
            ),
        });
    }
    copy_file(&file.transaction.target, &backup_path)
        .map_err(|error| write_error("backup copy", &backup_path, &error))?;
    sync_backup_file(
        &backup_path,
        file.source_permissions.as_ref(),
        index,
        faults,
    )?;
    let verified = read_file(&backup_path, "backup descriptor")?;
    if verified != *source {
        return Err(Error::WriteFailed {
            message: format!(
                "backup verification failed for {}; Unclean changed no target files",
                file.transaction.relative_path.display()
            ),
        });
    }
    Ok(())
}

fn sync_backup_file(
    path: &Path,
    permissions: Option<&Permissions>,
    index: usize,
    faults: &dyn FaultInjector,
) -> Result<()> {
    let readonly = permissions.is_some_and(Permissions::readonly);
    if readonly {
        set_readonly(path, false)
            .map_err(|error| write_error("backup attribute update", path, &error))?;
    }
    let sync_result = if faults.fails(Boundary::BackupSync, index) {
        Err(Error::WriteFailed {
            message: format!(
                "backup sync failed for {} by injected fault",
                path.display()
            ),
        })
    } else {
        OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| write_error("backup sync open", path, &error))
            .and_then(|file| {
                file.sync_all()
                    .map_err(|error| write_error("backup sync", path, &error))
            })
    };
    if readonly {
        set_readonly(path, true)
            .map_err(|error| write_error("backup attribute restoration", path, &error))?;
    }
    sync_result
}

fn manifest_file(file: &PreparedFile) -> BackupManifestFile {
    BackupManifestFile {
        relative_path: file.transaction.relative_path.clone(),
        source_existed: file.source_bytes.is_some(),
        sha256_before: file.source_bytes.as_deref().map(sha256_hex),
        sha256_after: file.transaction.planned_hash.clone(),
        value_before: file.transaction.value_before,
        value_after: file.transaction.value_after,
        source_length: file
            .source_bytes
            .as_ref()
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX)),
        planned_length: u64::try_from(file.transaction.planned_bytes.len()).unwrap_or(u64::MAX),
        readonly_before: file
            .source_permissions
            .as_ref()
            .is_some_and(Permissions::readonly),
    }
}

fn replace_target(
    transaction: &Transaction,
    file: &PreparedFile,
    index: usize,
    faults: &dyn FaultInjector,
) -> Result<()> {
    validate_transaction_target(transaction, &file.transaction)?;
    let current = match fs::read(&file.transaction.target) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => {
            return Err(read_error(
                "source descriptor",
                &file.transaction.target,
                &error,
            ));
        }
    };
    if current.as_deref().map(sha256_hex).as_deref()
        != file.transaction.expected_source_hash.as_deref()
    {
        return Err(Error::Conflict {
            message: format!(
                "source bytes changed before replacement for {}",
                file.transaction.relative_path.display()
            ),
        });
    }
    let temporary = temporary_path(
        &file.transaction.target,
        &transaction.operation_id,
        index,
        "write",
    )?;
    write_new_file(
        &temporary,
        &file.transaction.planned_bytes,
        Boundary::TemporaryShortWrite,
        Boundary::TemporarySync,
        index,
        faults,
    )?;
    if faults.fails(Boundary::Replacement, index) {
        return Err(Error::WriteFailed {
            message: format!(
                "replacement failed for {} by injected fault",
                file.transaction.relative_path.display()
            ),
        });
    }
    make_target_writable(&file.transaction.target, file.source_permissions.as_ref())?;
    let replacement = if file.source_bytes.is_some() {
        replace_file(&file.transaction.target, &temporary)
    } else {
        install_file(&file.transaction.target, &temporary)
    };
    replacement
        .map_err(|error| write_error("descriptor replacement", &file.transaction.target, &error))?;
    if let Some(permissions) = &file.source_permissions {
        fs::set_permissions(&file.transaction.target, permissions.clone()).map_err(|error| {
            write_error(
                "descriptor metadata restoration",
                &file.transaction.target,
                &error,
            )
        })?;
    }
    verify_target(
        &file.transaction,
        faults.fails(Boundary::Verification, index),
    )
}

fn rollback_after_failure(
    transaction: &Transaction,
    prepared: &[PreparedFile],
    written: &[usize],
    faults: &dyn FaultInjector,
    cause: Error,
) -> Result<OperationReport> {
    for index in written.iter().rev().copied() {
        if let Err(rollback_error) = rollback_file(transaction, &prepared[index], index, faults) {
            return Err(Error::RollbackIncomplete {
                message: format!(
                    "{}; rollback failed for {}: {}. Recover the file from {}",
                    cause,
                    prepared[index].transaction.target.display(),
                    rollback_error,
                    transaction
                        .backup_directory
                        .join(&prepared[index].transaction.relative_path)
                        .display()
                ),
            });
        }
    }
    Err(match cause {
        Error::PermissionDenied { .. } | Error::Conflict { .. } | Error::InvalidInput { .. } => {
            cause
        }
        _ => Error::WriteFailed {
            message: format!(
                "{}; Unclean restored {} replaced file(s) from {}",
                cause,
                written.len(),
                transaction.backup_directory.display()
            ),
        },
    })
}

fn rollback_file(
    transaction: &Transaction,
    file: &PreparedFile,
    index: usize,
    faults: &dyn FaultInjector,
) -> Result<()> {
    if faults.fails(Boundary::RollbackReplacement, index) {
        return Err(Error::WriteFailed {
            message: "rollback replacement failed by injected fault".to_owned(),
        });
    }
    if let Some(source) = &file.source_bytes {
        let temporary = temporary_path(
            &file.transaction.target,
            &transaction.operation_id,
            index,
            "rollback",
        )?;
        write_new_file(
            &temporary,
            source,
            Boundary::BackupWrite,
            Boundary::BackupSync,
            index,
            &NoFault,
        )?;
        make_target_writable(&file.transaction.target, file.source_permissions.as_ref())?;
        replace_file(&file.transaction.target, &temporary).map_err(|error| {
            write_error("rollback replacement", &file.transaction.target, &error)
        })?;
        if let Some(permissions) = &file.source_permissions {
            fs::set_permissions(&file.transaction.target, permissions.clone()).map_err(
                |error| {
                    write_error(
                        "rollback metadata restoration",
                        &file.transaction.target,
                        &error,
                    )
                },
            )?;
        }
        let restored = read_file(&file.transaction.target, "rolled-back descriptor")?;
        if restored != *source {
            return Err(Error::WriteFailed {
                message: "rolled-back bytes do not match the source backup".to_owned(),
            });
        }
    } else {
        fs::remove_file(&file.transaction.target)
            .map_err(|error| write_error("rollback removal", &file.transaction.target, &error))?;
    }
    Ok(())
}

fn verify_target(file: &TransactionFile, force_mismatch: bool) -> Result<()> {
    let bytes = read_file(&file.target, "written descriptor")?;
    if force_mismatch || sha256_hex(&bytes) != file.planned_hash {
        return Err(Error::WriteFailed {
            message: format!(
                "written bytes failed verification for {}",
                file.relative_path.display()
            ),
        });
    }
    verify_transaction_bytes(file, &bytes, true)
}

fn write_journal(path: &Path, state: &JournalState, faults: &dyn FaultInjector) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::InvalidInput {
        message: "journal path has no parent directory".to_owned(),
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| write_error("journal directory creation", parent, &error))?;
    let temporary = temporary_path(path, "journal", state.operations.len(), "state")?;
    write_new_file(
        &temporary,
        state.render().as_bytes(),
        Boundary::BackupWrite,
        Boundary::JournalSync,
        state.operations.len(),
        faults,
    )?;
    let result = if path.exists() {
        replace_file(path, &temporary)
    } else {
        install_file(path, &temporary)
    };
    result.map_err(|error| write_error("journal replacement", path, &error))?;
    let verified = load_journal(path)?;
    if verified != *state {
        return Err(Error::WriteFailed {
            message: format!("journal verification failed at {}", path.display()),
        });
    }
    Ok(())
}

fn write_new_file(
    path: &Path,
    bytes: &[u8],
    write_boundary: Boundary,
    sync_boundary: Boundary,
    index: usize,
    faults: &dyn FaultInjector,
) -> Result<()> {
    if faults.fails(write_boundary, index) && write_boundary != Boundary::TemporaryShortWrite {
        return Err(Error::WriteFailed {
            message: format!("write failed for {} by injected fault", path.display()),
        });
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| write_error("file creation", path, &error))?;
    if faults.fails(Boundary::TemporaryShortWrite, index)
        && write_boundary == Boundary::TemporaryShortWrite
    {
        let partial = bytes.len().saturating_div(2).max(1).min(bytes.len());
        file.write_all(&bytes[..partial])
            .map_err(|error| write_error("partial file write", path, &error))?;
        return Err(Error::WriteFailed {
            message: format!(
                "temporary write ended before completion for {}",
                path.display()
            ),
        });
    }
    file.write_all(bytes)
        .map_err(|error| write_error("file write", path, &error))?;
    if faults.fails(sync_boundary, index) {
        return Err(Error::WriteFailed {
            message: format!("file sync failed for {} by injected fault", path.display()),
        });
    }
    file.sync_all()
        .map_err(|error| write_error("file sync", path, &error))
}

fn make_target_writable(path: &Path, permissions: Option<&Permissions>) -> Result<()> {
    let Some(permissions) = permissions else {
        return Ok(());
    };
    if !permissions.readonly() {
        return Ok(());
    }
    set_readonly(path, false)
        .map_err(|error| write_error("read-only attribute update", path, &error))
}

fn temporary_path(target: &Path, operation_id: &str, index: usize, role: &str) -> Result<PathBuf> {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::InvalidInput {
            message: format!("target file name is invalid: {}", target.display()),
        })?;
    Ok(target.with_file_name(format!(".{name}.unclean-{operation_id}-{index}-{role}.tmp")))
}

fn read_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| read_error(label, path, &error))
}

fn read_error(label: &str, path: &Path, error: &std::io::Error) -> Error {
    match error.kind() {
        ErrorKind::NotFound => Error::NotFound {
            item: format!("{label} {}", path.display()),
        },
        ErrorKind::PermissionDenied => Error::PermissionDenied {
            message: format!("Unclean cannot read {label} {}", path.display()),
        },
        _ => Error::Internal {
            message: format!("{label} read failed at {}: {error}", path.display()),
        },
    }
}

fn write_error(label: &str, path: &Path, error: &std::io::Error) -> Error {
    if error.kind() == ErrorKind::PermissionDenied {
        Error::PermissionDenied {
            message: format!("Permission denied for {label} at {}", path.display()),
        }
    } else {
        Error::WriteFailed {
            message: format!("{label} failed at {}: {error}", path.display()),
        }
    }
}

fn time_label() -> Result<String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Internal {
            message: format!("the system clock cannot record the operation time: {error}"),
        })?;
    let seconds = i64::try_from(duration.as_secs()).map_err(|_| Error::Internal {
        message: "the system clock exceeds the supported journal range".to_owned(),
    })?;
    Ok(iso_utc_label(seconds, duration.subsec_nanos()))
}

fn iso_utc_label(seconds: i64, nanoseconds: u32) -> String {
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let provisional_year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_phase = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_phase + 2) / 5 + 1;
    let month = month_phase + if month_phase < 10 { 3 } else { -9 };
    let year = provisional_year + i64::from(month <= 2);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanoseconds:09}Z")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::error::Error as StdError;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{
        Boundary, FaultInjector, WriteConfirmation, apply_engine_plan, apply_engine_plans,
        apply_project_plan, apply_template_plan, build_project_restore_plan, build_restore_plan,
        build_template_restore_plan, execute_transaction, iso_utc_label, restore_engine_plan,
        restore_project_plan, restore_template_plan, write_confirmation,
    };
    use crate::backups::load_backup_manifest;
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use crate::journal::{
        JournalOperationKind, OperationTargetKind, inspect_project_status, inspect_template_status,
        load_journal,
    };
    use crate::plans::{PlanBuildOptions, build_engine_plan};
    use crate::presets::{PRESET_SCHEMA, Preset};
    use crate::project_plans::build_project_edit_plan;
    use crate::projects::{
        ProjectDescriptorDocument, ProjectDescriptorEdit, ProjectPluginEdit,
        ProjectPluginEditAction, ProjectSuppressionEdit,
    };
    use crate::templates::build_template_plan;
    use crate::{ErrorCode, plans::sha256_hex};

    struct Faults(BTreeSet<(Boundary, usize)>);

    impl Faults {
        fn at(boundary: Boundary, index: usize) -> Self {
            Self(BTreeSet::from([(boundary, index)]))
        }
    }

    impl FaultInjector for Faults {
        fn fails(&self, boundary: Boundary, index: usize) -> bool {
            self.0.contains(&(boundary, index))
        }
    }

    #[test]
    fn interactive_writes_require_a_prompt() {
        assert!(matches!(
            write_confirmation(true, false),
            Ok(WriteConfirmation::PromptRequired)
        ));
    }

    #[test]
    fn yes_confirms_interactive_and_noninteractive_writes() {
        assert!(matches!(
            write_confirmation(true, true),
            Ok(WriteConfirmation::Confirmed)
        ));
        assert!(matches!(
            write_confirmation(false, true),
            Ok(WriteConfirmation::Confirmed)
        ));
    }

    #[test]
    fn noninteractive_writes_without_yes_fail_as_invalid_input() {
        let error = write_confirmation(false, false).err();

        assert_eq!(
            error.as_ref().map(crate::Error::code),
            Some(ErrorCode::InvalidInput)
        );
        assert!(error.is_some_and(|value| value.to_string().contains("--yes")));
    }

    #[test]
    fn journal_time_uses_a_utc_iso_label() {
        assert_eq!(iso_utc_label(0, 0), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(
            iso_utc_label(1_785_249_062, 123_456_789),
            "2026-07-28T14:31:02.123456789Z"
        );
    }

    #[test]
    fn successful_apply_writes_backup_manifest_and_journal() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("success")?;
        let plan = fixture.plan()?;
        let report = apply_engine_plan(&plan, &fixture.journal_path)?;

        assert!(report.recorded);
        assert_eq!(report.files_written, 2);
        assert_eq!(load_journal(&fixture.journal_path)?.operations.len(), 1);
        assert!(plan.backup_directory().join("manifest.toml").is_file());
        for edit in plan.changes() {
            assert_eq!(fs::read(&edit.path)?, edit.planned_bytes());
            assert_eq!(
                sha256_hex(&fs::read(
                    plan.backup_directory().join(&edit.relative_path)
                )?),
                edit.sha256_before
            );
        }
        Ok(())
    }

    #[test]
    fn apply_preserves_the_read_only_attribute_on_target_and_backup()
    -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("read-only")?;
        let plan = fixture.plan()?;
        let edit = plan.changes().first().ok_or("plan has no edit")?;
        super::set_readonly(&edit.path, true)?;

        apply_engine_plan(&plan, &fixture.journal_path)?;

        let backup = plan.backup_directory().join(&edit.relative_path);
        assert!(fs::metadata(&edit.path)?.permissions().readonly());
        assert!(fs::metadata(&backup)?.permissions().readonly());
        super::set_readonly(&edit.path, false)?;
        super::set_readonly(&backup, false)?;
        Ok(())
    }

    #[test]
    fn filesystem_faults_leave_original_target_bytes() -> Result<(), Box<dyn StdError>> {
        for (case, boundary, index) in [
            ("backup-write", Boundary::BackupWrite, 0),
            ("backup-sync", Boundary::BackupSync, 0),
            ("manifest-sync", Boundary::ManifestSync, 2),
            ("short-write", Boundary::TemporaryShortWrite, 0),
            ("temp-sync", Boundary::TemporarySync, 0),
            ("replace", Boundary::Replacement, 0),
            ("verify", Boundary::Verification, 0),
            ("journal", Boundary::JournalSync, 1),
        ] {
            let fixture = fixture(case)?;
            let plan = fixture.plan()?;
            let original = fixture.current_bytes()?;
            let transaction = super::Transaction::from_engine_plan(&plan);

            let error = execute_transaction(
                &transaction,
                &fixture.journal_path,
                &Faults::at(boundary, index),
            )
            .err();

            assert!(error.is_some(), "{case} did not fail");
            assert_eq!(fixture.current_bytes()?, original, "{case} changed bytes");
            assert!(
                load_journal(&fixture.journal_path)?.operations.is_empty(),
                "{case} recorded a failed operation"
            );
        }
        Ok(())
    }

    #[test]
    fn rollback_failure_reports_manual_recovery_path() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("rollback-failure")?;
        let plan = fixture.plan()?;
        let transaction = super::Transaction::from_engine_plan(&plan);
        let faults = Faults(BTreeSet::from([
            (Boundary::Replacement, 1),
            (Boundary::RollbackReplacement, 0),
        ]));

        let error = execute_transaction(&transaction, &fixture.journal_path, &faults)
            .err()
            .ok_or("faults did not fail")?;

        assert_eq!(error.code(), ErrorCode::RollbackIncomplete);
        assert!(
            error
                .to_string()
                .contains(&plan.backup_directory().display().to_string())
        );
        Ok(())
    }

    #[test]
    fn restore_reuses_the_writer_and_records_its_source_snapshot() -> Result<(), Box<dyn StdError>>
    {
        let fixture = fixture("restore")?;
        let original = fixture.current_bytes()?;
        let apply_plan = fixture.plan()?;
        apply_engine_plan(&apply_plan, &fixture.journal_path)?;
        let restore_options = PlanBuildOptions::new(
            fixture.temp.path().join("restore-backups"),
            "test-restore-operation".to_owned(),
        )?;
        let restore_plan = build_restore_plan(
            &fixture.engine,
            apply_plan.operation_id(),
            &fixture.journal_path,
            &restore_options,
        )?;

        let report = restore_engine_plan(&restore_plan, &fixture.journal_path)?;

        assert_eq!(report.kind, JournalOperationKind::Restore);
        assert_eq!(fixture.current_bytes()?, original);
        let journal = load_journal(&fixture.journal_path)?;
        assert_eq!(journal.operations.len(), 2);
        assert_eq!(
            journal.operations[1].source_snapshot.as_deref(),
            Some(apply_plan.operation_id())
        );
        assert!(
            restore_plan
                .backup_directory()
                .join("manifest.toml")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn project_apply_and_restore_share_backup_verification_and_history()
    -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_5.8");
        write_plugin(
            &engine_path,
            "Runtime/DefaultPlugin/DefaultPlugin.uplugin",
            br#"{"FileVersion":3,"EnabledByDefault":true}"#,
        )?;
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.8.0-test".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 1,
            issues: Vec::new(),
        };
        let project_path = temp.path().join("Fixture.uproject");
        let original = b"\xEF\xBB\xBF{\r\n  // Preserved project field.\r\n  \"FileVersion\": 3,\r\n  \"EngineAssociation\": \"5.8\",\r\n  \"Category\": \"Games\",\r\n}\r\n";
        fs::write(&project_path, original)?;
        let journal_path = temp.path().join("state.toml");
        let apply_options =
            PlanBuildOptions::new(temp.path().join("backups"), "project-apply".to_owned())?;
        let plan = build_project_edit_plan(
            &project_path,
            std::slice::from_ref(&engine),
            "Manual project edit",
            ProjectDescriptorEdit {
                suppression: ProjectSuppressionEdit::Keep,
                plugins: vec![ProjectPluginEdit {
                    plugin: "DefaultPlugin".to_owned(),
                    action: ProjectPluginEditAction::Disable,
                }],
            },
            &apply_options,
        )?;

        let report = apply_project_plan(&plan, &journal_path)?;

        assert_eq!(report.target_kind, OperationTargetKind::Project);
        assert_eq!(report.project_path.as_deref(), Some(plan.project_path()));
        assert_eq!(report.files_written, 1);
        assert!(report.recorded);
        let written = ProjectDescriptorDocument::load(&project_path)?;
        assert_eq!(
            written
                .plugins()
                .iter()
                .find(|plugin| plugin.name == "DefaultPlugin")
                .map(|plugin| plugin.enabled),
            Some(false)
        );
        let project_file_name = project_path.file_name().ok_or("project has no file name")?;
        assert_eq!(
            fs::read(plan.backup_directory().join(project_file_name))?,
            original
        );
        let manifest = load_backup_manifest(plan.backup_directory())?;
        assert_eq!(manifest.target_kind, OperationTargetKind::Project);
        assert_eq!(manifest.project_path.as_deref(), Some(plan.project_path()));
        assert!(manifest.preset_path.is_none());
        let journal = load_journal(&journal_path)?;
        assert_eq!(journal.operations.len(), 1);
        assert_eq!(
            journal.operations[0].target_kind,
            OperationTargetKind::Project
        );
        assert_eq!(
            journal.operations[0].project_path.as_deref(),
            Some(plan.project_path())
        );
        let status = inspect_project_status(plan.project_path(), &journal_path)?;
        assert!(status.recorded);
        assert!(!status.drifted);

        let restore_options = PlanBuildOptions::new(
            temp.path().join("restore-backups"),
            "project-restore".to_owned(),
        )?;
        let restore_plan = build_project_restore_plan(
            &project_path,
            std::slice::from_ref(&engine),
            plan.operation_id(),
            &journal_path,
            &restore_options,
        )?;
        let restore_report = restore_project_plan(&restore_plan, &journal_path)?;

        assert_eq!(restore_report.kind, JournalOperationKind::Restore);
        assert_eq!(restore_report.target_kind, OperationTargetKind::Project);
        assert_eq!(fs::read(&project_path)?, original);
        let journal = load_journal(&journal_path)?;
        assert_eq!(journal.operations.len(), 2);
        assert_eq!(
            journal.operations[1].source_snapshot.as_deref(),
            Some(plan.operation_id())
        );
        assert_eq!(
            journal.operations[1].project_path.as_deref(),
            Some(plan.project_path())
        );
        assert!(!inspect_project_status(plan.project_path(), &journal_path)?.drifted);
        Ok(())
    }

    #[test]
    fn template_apply_and_restore_share_the_protected_transaction() -> Result<(), Box<dyn StdError>>
    {
        let root = tempdir()?;
        let engine_path = root.path().join("UE_Template");
        let template_path = engine_path.join("Templates/TP_Blank/TP_Blank.uproject");
        fs::create_dir_all(engine_path.join("Templates/TP_Blank"))?;
        let source = b"{\r\n\t\"FileVersion\": 3,\r\n\t\"Plugins\": [{\"Name\":\"KeepMe\",\"Enabled\":true}],\r\n}\r\n";
        fs::write(&template_path, source)?;
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.8.1".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 0,
            issues: Vec::new(),
        };
        let backup_root = root.path().join("backups");
        let journal_path = root.path().join("state.toml");
        let apply_options =
            PlanBuildOptions::new(backup_root.clone(), "template-apply".to_owned())?;
        let plan = build_template_plan(
            &engine,
            &[PathBuf::from("Templates/TP_Blank/TP_Blank.uproject")],
            ProjectSuppressionEdit::Set(true),
            &apply_options,
        )?;

        let report = apply_template_plan(&plan, &journal_path)?;

        assert_eq!(report.target_kind, OperationTargetKind::Template);
        assert_eq!(report.files_written, 1);
        assert!(
            String::from_utf8(fs::read(&template_path)?)?
                .contains("\"DisableEnginePluginsByDefault\": true")
        );
        let manifest = load_backup_manifest(plan.backup_directory())?;
        assert_eq!(manifest.target_kind, OperationTargetKind::Template);
        let status = inspect_template_status(&engine, &journal_path)?;
        assert_eq!(
            status
                .operation
                .as_ref()
                .map(|operation| operation.id.as_str()),
            Some("template-apply")
        );

        let restore_options = PlanBuildOptions::new(backup_root, "template-restore".to_owned())?;
        let restore = build_template_restore_plan(
            &engine,
            "template-apply",
            &journal_path,
            &restore_options,
        )?;
        let restore_report = restore_template_plan(&restore, &journal_path)?;

        assert_eq!(restore_report.target_kind, OperationTargetKind::Template);
        assert_eq!(fs::read(template_path)?, source);
        Ok(())
    }

    #[test]
    fn multi_engine_apply_stops_after_the_first_failed_engine() -> Result<(), Box<dyn StdError>> {
        let first = fixture("multi-first")?;
        let second = fixture("multi-second")?;
        let third = fixture("multi-third")?;
        let first_plan = first.plan()?;
        let second_plan = second.plan()?;
        let third_plan = third.plan()?;
        let first_expected = first_plan
            .changes()
            .iter()
            .map(|edit| edit.planned_bytes().to_vec())
            .collect::<Vec<_>>();
        let third_original = third.current_bytes()?;
        fs::write(&second.targets[0], b"changed after planning")?;

        let error =
            apply_engine_plans(&[first_plan, second_plan, third_plan], &first.journal_path).err();

        assert!(error.is_some());
        assert_eq!(first.current_bytes()?, first_expected);
        assert_eq!(third.current_bytes()?, third_original);
        assert_eq!(load_journal(&first.journal_path)?.operations.len(), 1);
        Ok(())
    }

    struct Fixture {
        temp: tempfile::TempDir,
        engine: EngineInstallation,
        preset_path: PathBuf,
        preset: Preset,
        options: PlanBuildOptions,
        journal_path: PathBuf,
        targets: Vec<PathBuf>,
    }

    impl Fixture {
        fn plan(&self) -> crate::Result<crate::plans::EnginePlan> {
            build_engine_plan(&self.engine, &self.preset_path, &self.preset, &self.options)
        }

        fn current_bytes(&self) -> std::io::Result<Vec<Vec<u8>>> {
            self.targets.iter().map(fs::read).collect()
        }
    }

    fn fixture(case: &str) -> Result<Fixture, Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_Invented");
        let first = write_plugin(
            &engine_path,
            "Runtime/First/First.uplugin",
            br#"{"FileVersion":3,"EnabledByDefault":true}"#,
        )?;
        let second = write_plugin(
            &engine_path,
            "Runtime/Second/Second.uplugin",
            br#"{"FileVersion":3,"EnabledByDefault":true}"#,
        )?;
        let preset_path = temp.path().join("preset.toml");
        fs::write(&preset_path, "fixture")?;
        let preset = Preset {
            schema: PRESET_SCHEMA,
            name: "Fixture".to_owned(),
            description: None,
            enable: Vec::new(),
            disable: vec!["First".to_owned(), "Second".to_owned()],
            clear: Vec::new(),
            disable_matching: Vec::new(),
        };
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.9.0-test".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 2,
            issues: Vec::new(),
        };
        Ok(Fixture {
            options: PlanBuildOptions::new(temp.path().join("backups"), format!("test-{case}"))?,
            journal_path: temp.path().join("state.toml"),
            temp,
            engine,
            preset_path,
            preset,
            targets: vec![first, second],
        })
    }

    fn write_plugin(
        engine: &Path,
        relative: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, Box<dyn StdError>> {
        let path = engine.join("Engine").join("Plugins").join(relative);
        fs::create_dir_all(path.parent().ok_or("plugin path has no parent")?)?;
        fs::write(&path, bytes)?;
        Ok(path)
    }
}
