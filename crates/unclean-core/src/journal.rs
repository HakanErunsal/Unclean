//! Records completed operations, backup locations, verified hashes, and restore results.

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::discovery::EngineInstallation;
use crate::plans::sha256_hex;
use crate::{Error, Result};

/// Identifies the journal schema accepted by this build.
pub const JOURNAL_SCHEMA: i64 = 1;

/// Stores completed operations from `%APPDATA%\Unclean\state.toml`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct JournalState {
    /// Identifies the journal schema.
    pub schema: i64,
    /// Retains completed operations in write order.
    pub operations: Vec<JournalOperation>,
}

impl Default for JournalState {
    fn default() -> Self {
        Self {
            schema: JOURNAL_SCHEMA,
            operations: Vec::new(),
        }
    }
}

impl JournalState {
    /// Renders the complete journal as schema 1 TOML.
    #[must_use]
    pub fn render(&self) -> String {
        let mut document = DocumentMut::new();
        document["schema"] = value(self.schema);
        let mut operations = ArrayOfTables::new();
        for operation in &self.operations {
            operations.push(operation_table(operation));
        }
        document["operation"] = Item::ArrayOfTables(operations);
        document.to_string()
    }

    /// Parses a journal after validating its schema, target paths, and hashes.
    ///
    /// # Errors
    ///
    /// Returns an error when the TOML or a required operation field is invalid.
    pub fn parse(source: &str) -> Result<Self> {
        let document = source
            .parse::<DocumentMut>()
            .map_err(|error| Error::InvalidInput {
                message: format!("journal TOML is invalid: {error}"),
            })?;
        reject_unknown_document_fields(&document)?;
        let schema = document
            .get("schema")
            .and_then(Item::as_integer)
            .ok_or_else(|| Error::InvalidInput {
                message: "journal field \"schema\" must contain an integer".to_owned(),
            })?;
        if schema != JOURNAL_SCHEMA {
            return Err(Error::InvalidInput {
                message: format!(
                    "this build supports journal schema {JOURNAL_SCHEMA}; file uses schema {schema}"
                ),
            });
        }
        let operations = match document.get("operation") {
            Some(item) => item
                .as_array_of_tables()
                .ok_or_else(|| Error::InvalidInput {
                    message: "journal field \"operation\" must contain tables".to_owned(),
                })?
                .iter()
                .enumerate()
                .map(|(index, table)| parse_operation(table, index))
                .collect::<Result<Vec<_>>>()?,
            None => Vec::new(),
        };
        Ok(Self { schema, operations })
    }
}

/// Identifies the completed write recorded in one journal entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOperationKind {
    /// Records a preset apply.
    Apply,
    /// Records a snapshot restore.
    Restore,
}

impl JournalOperationKind {
    /// Returns the stable lowercase value used in journal output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Restore => "restore",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "apply" => Ok(Self::Apply),
            "restore" => Ok(Self::Restore),
            _ => Err(Error::InvalidInput {
                message: format!("journal schema 1 does not support operation kind \"{value}\""),
            }),
        }
    }
}

/// Identifies the descriptor boundary for one operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationTargetKind {
    /// Targets `.uplugin` files under one engine root.
    Engine,
    /// Targets one selected `.uproject` file.
    Project,
    /// Targets selected `.uproject` files under one engine `Templates` directory.
    Template,
}

impl OperationTargetKind {
    /// Returns the stable lowercase value used in journal and backup output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Engine => "engine",
            Self::Project => "project",
            Self::Template => "template",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "engine" => Ok(Self::Engine),
            "project" => Ok(Self::Project),
            "template" => Ok(Self::Template),
            _ => Err(Error::InvalidInput {
                message: format!("journal schema 1 does not support target kind \"{value}\""),
            }),
        }
    }
}

/// Records one completed engine or project operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct JournalOperation {
    /// Identifies the operation and its backup directory.
    pub id: String,
    /// Identifies whether the operation applied a preset or restored a snapshot.
    pub kind: JournalOperationKind,
    /// Identifies the descriptor boundary used by the operation.
    pub target_kind: OperationTargetKind,
    /// Records the canonical engine path used by the operation.
    pub engine_path: PathBuf,
    /// Records the engine version when discovery supplied one.
    pub engine_version: Option<String>,
    /// Records the selected project descriptor for project operations.
    pub project_path: Option<PathBuf>,
    /// Names the applied preset.
    pub preset: String,
    /// Records the completion timestamp or stable time label.
    pub completed: String,
    /// Records the full backup directory for recovery.
    pub backup_directory: PathBuf,
    /// Identifies the source snapshot for a restore operation.
    pub source_snapshot: Option<String>,
    /// Retains the verified post-write hash for each target.
    pub files: Vec<JournalFile>,
}

/// Records one target from a completed operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct JournalFile {
    /// Records the descriptor path relative to its engine or project boundary.
    pub relative_path: PathBuf,
    /// Records the verified post-write SHA-256 digest.
    pub sha256_after: String,
}

/// Identifies the current disk condition for one recorded target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedFileState {
    /// Reports that current bytes match the recorded post-write hash.
    Matching,
    /// Reports that current bytes differ from the recorded post-write hash.
    Modified,
    /// Reports that the recorded target no longer exists.
    Missing,
    /// Reports an unreadable recorded target.
    Unreadable,
}

impl RecordedFileState {
    /// Returns the stable lowercase label used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Matching => "matching",
            Self::Modified => "modified",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
        }
    }

    const fn is_drift(self) -> bool {
        !matches!(self, Self::Matching)
    }
}

/// Reports the current hash condition for one recorded target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RecordedFileStatus {
    /// Records the target path relative to its engine or project boundary.
    pub relative_path: PathBuf,
    /// Retains the verified hash from the completed operation.
    pub expected_sha256: String,
    /// Reports the current hash when the target could be read.
    pub actual_sha256: Option<String>,
    /// Identifies the current disk condition.
    pub state: RecordedFileState,
    /// States a read failure when no current hash is available.
    pub message: Option<String>,
}

/// Identifies the operation used for one status result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StatusOperation {
    /// Identifies the recorded operation.
    pub id: String,
    /// Names the applied preset.
    pub preset: String,
    /// Records the completion timestamp or stable time label.
    pub completed: String,
    /// Records the backup directory available for recovery.
    pub backup_directory: PathBuf,
}

/// Reports recorded drift for the latest operation on one engine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EngineStatus {
    /// Reports whether the journal contains a completed operation for this engine.
    pub recorded: bool,
    /// Reports whether any recorded target differs from disk.
    pub drifted: bool,
    /// Identifies the latest matching journal operation.
    pub operation: Option<StatusOperation>,
    /// Lists every recorded target and its current hash condition.
    pub files: Vec<RecordedFileStatus>,
}

/// Reports recorded drift for the latest operation on one project.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectStatus {
    /// Reports whether the journal contains a completed operation for this project.
    pub recorded: bool,
    /// Reports whether the recorded project bytes differ from disk.
    pub drifted: bool,
    /// Identifies the latest matching journal operation.
    pub operation: Option<StatusOperation>,
    /// Lists the recorded project target and its current hash condition.
    pub files: Vec<RecordedFileStatus>,
}

/// Returns `%APPDATA%\Unclean\state.toml` without creating the file.
///
/// # Errors
///
/// Returns an error when `APPDATA` is missing or empty.
pub fn default_journal_path() -> Result<PathBuf> {
    env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("Unclean").join("state.toml"))
        .ok_or_else(|| Error::InvalidInput {
            message: "Unclean cannot locate APPDATA for journal lookup".to_owned(),
        })
}

/// Loads a journal and treats a missing file as an empty schema 1 state.
///
/// # Errors
///
/// Returns an error when journal loading or parsing fails.
pub fn load_journal(path: &Path) -> Result<JournalState> {
    match fs::read_to_string(path) {
        Ok(source) => JournalState::parse(&source),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(JournalState::default()),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => Err(Error::PermissionDenied {
            message: format!("Unclean cannot read journal {}", path.display()),
        }),
        Err(error) => Err(Error::Internal {
            message: format!("journal read failed: {error}"),
        }),
    }
}

/// Returns completed operations for one engine in newest-first order.
///
/// # Errors
///
/// Returns an error when the journal is invalid.
pub fn engine_history(
    engine: &EngineInstallation,
    journal_path: &Path,
) -> Result<Vec<JournalOperation>> {
    let state = load_journal(journal_path)?;
    Ok(state
        .operations
        .into_iter()
        .rev()
        .filter(|operation| {
            operation.target_kind == OperationTargetKind::Engine
                && paths_match(&operation.engine_path, &engine.path)
        })
        .collect())
}

/// Returns completed operations for one project in newest-first order.
///
/// # Errors
///
/// Returns an error when the journal is invalid.
pub fn project_history(project_path: &Path, journal_path: &Path) -> Result<Vec<JournalOperation>> {
    let state = load_journal(journal_path)?;
    Ok(state
        .operations
        .into_iter()
        .rev()
        .filter(|operation| {
            operation.target_kind == OperationTargetKind::Project
                && operation
                    .project_path
                    .as_ref()
                    .is_some_and(|path| paths_match(path, project_path))
        })
        .collect())
}

/// Returns completed template operations for one engine in newest-first order.
///
/// # Errors
///
/// Returns an error when the journal is invalid.
pub fn template_history(
    engine: &EngineInstallation,
    journal_path: &Path,
) -> Result<Vec<JournalOperation>> {
    let state = load_journal(journal_path)?;
    Ok(state
        .operations
        .into_iter()
        .rev()
        .filter(|operation| {
            operation.target_kind == OperationTargetKind::Template
                && paths_match(&operation.engine_path, &engine.path)
        })
        .collect())
}

/// Compares the latest recorded operation for one engine with current descriptor bytes.
///
/// # Errors
///
/// Returns an error when the journal is invalid or contains a target outside the selected engine.
pub fn inspect_engine_status(
    engine: &EngineInstallation,
    journal_path: &Path,
) -> Result<EngineStatus> {
    let state = load_journal(journal_path)?;
    let Some(operation) = state.operations.iter().rev().find(|operation| {
        operation.target_kind == OperationTargetKind::Engine
            && paths_match(&operation.engine_path, &engine.path)
    }) else {
        return Ok(EngineStatus {
            recorded: false,
            drifted: false,
            operation: None,
            files: Vec::new(),
        });
    };

    let mut files = Vec::with_capacity(operation.files.len());
    for file in &operation.files {
        validate_relative_target_path(operation.target_kind, &file.relative_path)?;
        let path = engine.path.join(&file.relative_path);
        files.push(inspect_recorded_file(
            file,
            &path,
            OperationTargetKind::Engine,
        ));
    }
    let drifted = files.iter().any(|file| file.state.is_drift());
    Ok(EngineStatus {
        recorded: true,
        drifted,
        operation: Some(StatusOperation {
            id: operation.id.clone(),
            preset: operation.preset.clone(),
            completed: operation.completed.clone(),
            backup_directory: operation.backup_directory.clone(),
        }),
        files,
    })
}

/// Compares the latest recorded operation for one project with its current descriptor bytes.
///
/// # Errors
///
/// Returns an error when the journal is invalid or the recorded target differs from the selected project.
pub fn inspect_project_status(project_path: &Path, journal_path: &Path) -> Result<ProjectStatus> {
    let state = load_journal(journal_path)?;
    let Some(operation) = state.operations.iter().rev().find(|operation| {
        operation.target_kind == OperationTargetKind::Project
            && operation
                .project_path
                .as_ref()
                .is_some_and(|path| paths_match(path, project_path))
    }) else {
        return Ok(ProjectStatus {
            recorded: false,
            drifted: false,
            operation: None,
            files: Vec::new(),
        });
    };
    let parent = project_path.parent().ok_or_else(|| Error::InvalidInput {
        message: format!(
            "selected project has no parent directory: {}",
            project_path.display()
        ),
    })?;
    let mut files = Vec::with_capacity(operation.files.len());
    for file in &operation.files {
        validate_relative_target_path(OperationTargetKind::Project, &file.relative_path)?;
        let path = parent.join(&file.relative_path);
        if !paths_match(&path, project_path) {
            return Err(Error::Conflict {
                message: format!(
                    "recorded project target differs from the selected project: {}",
                    file.relative_path.display()
                ),
            });
        }
        files.push(inspect_recorded_file(
            file,
            &path,
            OperationTargetKind::Project,
        ));
    }
    let drifted = files.iter().any(|file| file.state.is_drift());
    Ok(ProjectStatus {
        recorded: true,
        drifted,
        operation: Some(StatusOperation {
            id: operation.id.clone(),
            preset: operation.preset.clone(),
            completed: operation.completed.clone(),
            backup_directory: operation.backup_directory.clone(),
        }),
        files,
    })
}

/// Compares the latest template operation for one engine with current descriptor bytes.
///
/// # Errors
///
/// Returns an error when the journal is invalid or contains a target outside the template boundary.
pub fn inspect_template_status(
    engine: &EngineInstallation,
    journal_path: &Path,
) -> Result<EngineStatus> {
    let state = load_journal(journal_path)?;
    let Some(operation) = state.operations.iter().rev().find(|operation| {
        operation.target_kind == OperationTargetKind::Template
            && paths_match(&operation.engine_path, &engine.path)
    }) else {
        return Ok(EngineStatus {
            recorded: false,
            drifted: false,
            operation: None,
            files: Vec::new(),
        });
    };
    let mut files = Vec::with_capacity(operation.files.len());
    for file in &operation.files {
        validate_relative_target_path(OperationTargetKind::Template, &file.relative_path)?;
        files.push(inspect_recorded_file(
            file,
            &engine.path.join(&file.relative_path),
            OperationTargetKind::Template,
        ));
    }
    let drifted = files.iter().any(|file| file.state.is_drift());
    Ok(EngineStatus {
        recorded: true,
        drifted,
        operation: Some(StatusOperation {
            id: operation.id.clone(),
            preset: operation.preset.clone(),
            completed: operation.completed.clone(),
            backup_directory: operation.backup_directory.clone(),
        }),
        files,
    })
}

fn reject_unknown_document_fields(document: &DocumentMut) -> Result<()> {
    for (key, _) in document.iter() {
        if !matches!(key, "schema" | "operation") {
            return Err(Error::InvalidInput {
                message: format!("journal field \"{key}\" is not supported by schema 1"),
            });
        }
    }
    Ok(())
}

fn operation_table(operation: &JournalOperation) -> Table {
    let mut table = Table::new();
    table["id"] = value(operation.id.as_str());
    table["kind"] = value(operation.kind.as_str());
    if operation.target_kind != OperationTargetKind::Engine {
        table["target_kind"] = value(operation.target_kind.as_str());
    }
    table["engine_path"] = value(operation.engine_path.to_string_lossy().as_ref());
    if let Some(version) = &operation.engine_version {
        table["engine_version"] = value(version.as_str());
    }
    if let Some(project_path) = &operation.project_path {
        table["project_path"] = value(project_path.to_string_lossy().as_ref());
    }
    table["preset"] = value(operation.preset.as_str());
    table["completed"] = value(operation.completed.as_str());
    table["backup_directory"] = value(operation.backup_directory.to_string_lossy().as_ref());
    if let Some(snapshot) = &operation.source_snapshot {
        table["source_snapshot"] = value(snapshot.as_str());
    }
    let mut files = ArrayOfTables::new();
    for file in &operation.files {
        let mut file_table = Table::new();
        file_table["relative"] = value(file.relative_path.to_string_lossy().as_ref());
        file_table["sha256_after"] = value(file.sha256_after.as_str());
        files.push(file_table);
    }
    table["file"] = Item::ArrayOfTables(files);
    table
}

fn parse_operation(table: &Table, index: usize) -> Result<JournalOperation> {
    reject_unknown_table_fields(
        table,
        &[
            "id",
            "kind",
            "target_kind",
            "engine_path",
            "engine_version",
            "project_path",
            "preset",
            "completed",
            "backup_directory",
            "source_snapshot",
            "file",
        ],
        &format!("operation {index}"),
    )?;
    let id = required_string(table, "id", index)?;
    if !valid_identifier(&id) {
        return Err(Error::InvalidInput {
            message: format!("journal operation {index} has an invalid identifier"),
        });
    }
    let engine_path = PathBuf::from(required_string(table, "engine_path", index)?);
    if !engine_path.is_absolute() {
        return Err(Error::InvalidInput {
            message: format!("journal operation {index} has a relative engine path"),
        });
    }
    let backup_directory = PathBuf::from(required_string(table, "backup_directory", index)?);
    if !backup_directory.is_absolute() {
        return Err(Error::InvalidInput {
            message: format!("journal operation {index} has a relative backup directory"),
        });
    }
    let engine_version = optional_string(table, "engine_version", index)?;
    let target_kind = optional_string(table, "target_kind", index)?
        .map(|value| OperationTargetKind::parse(&value))
        .transpose()?
        .unwrap_or(OperationTargetKind::Engine);
    let project_path = optional_string(table, "project_path", index)?.map(PathBuf::from);
    match (target_kind, &project_path) {
        (OperationTargetKind::Engine | OperationTargetKind::Template, None) => {}
        (OperationTargetKind::Project, Some(path)) if path.is_absolute() => {}
        (OperationTargetKind::Engine | OperationTargetKind::Template, Some(_)) => {
            return Err(Error::InvalidInput {
                message: format!(
                    "journal operation {index} records a project path for a non-project target"
                ),
            });
        }
        (OperationTargetKind::Project, _) => {
            return Err(Error::InvalidInput {
                message: format!("journal operation {index} requires an absolute project path"),
            });
        }
    }
    let files = match table.get("file") {
        Some(item) => item
            .as_array_of_tables()
            .ok_or_else(|| Error::InvalidInput {
                message: format!("journal operation {index} field \"file\" must contain tables"),
            })?
            .iter()
            .enumerate()
            .map(|(file_index, file)| parse_file(file, index, file_index, target_kind))
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    Ok(JournalOperation {
        id,
        kind: optional_string(table, "kind", index)?
            .map(|value| JournalOperationKind::parse(&value))
            .transpose()?
            .unwrap_or(JournalOperationKind::Apply),
        target_kind,
        engine_path,
        engine_version,
        project_path,
        preset: required_string(table, "preset", index)?,
        completed: required_string(table, "completed", index)?,
        backup_directory,
        source_snapshot: optional_string(table, "source_snapshot", index)?,
        files,
    })
}

fn parse_file(
    table: &Table,
    operation_index: usize,
    file_index: usize,
    target_kind: OperationTargetKind,
) -> Result<JournalFile> {
    let context = format!("operation {operation_index} file {file_index}");
    reject_unknown_table_fields(table, &["relative", "sha256_after"], &context)?;
    let relative_path = PathBuf::from(required_context_string(table, "relative", &context)?);
    validate_relative_target_path(target_kind, &relative_path)?;
    let sha256_after = required_context_string(table, "sha256_after", &context)?;
    if !valid_sha256(&sha256_after) {
        return Err(Error::InvalidInput {
            message: format!("journal {context} has an invalid SHA-256 digest"),
        });
    }
    Ok(JournalFile {
        relative_path,
        sha256_after: sha256_after.to_ascii_lowercase(),
    })
}

fn reject_unknown_table_fields(table: &Table, accepted: &[&str], context: &str) -> Result<()> {
    for (key, _) in table {
        if !accepted.contains(&key) {
            return Err(Error::InvalidInput {
                message: format!("journal {context} field \"{key}\" is not supported by schema 1"),
            });
        }
    }
    Ok(())
}

fn required_string(table: &Table, key: &str, operation_index: usize) -> Result<String> {
    required_context_string(table, key, &format!("operation {operation_index}"))
}

fn required_context_string(table: &Table, key: &str, context: &str) -> Result<String> {
    table
        .get(key)
        .and_then(Item::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput {
            message: format!("journal {context} field \"{key}\" must contain a nonempty string"),
        })
}

fn optional_string(table: &Table, key: &str, operation_index: usize) -> Result<Option<String>> {
    table
        .get(key)
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidInput {
                    message: format!(
                        "journal operation {operation_index} field \"{key}\" must contain a string"
                    ),
                })
        })
        .transpose()
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
}

pub(crate) fn validate_relative_plugin_path(path: &Path) -> Result<()> {
    validate_relative_target_path(OperationTargetKind::Engine, path)
}

pub(crate) fn validate_relative_target_path(
    target_kind: OperationTargetKind,
    path: &Path,
) -> Result<()> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_)
                    | Component::RootDir
                    | Component::ParentDir
                    | Component::CurDir
            )
        })
    {
        return Err(Error::InvalidInput {
            message: format!(
                "journal target is not a safe {}-relative path: {}",
                target_kind.as_str(),
                path.display(),
            ),
        });
    }
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let valid = match target_kind {
        OperationTargetKind::Engine => {
            components.len() >= 3
                && components[0].eq_ignore_ascii_case("Engine")
                && components[1].eq_ignore_ascii_case("Plugins")
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("uplugin"))
        }
        OperationTargetKind::Project => {
            components.len() == 1
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        }
        OperationTargetKind::Template => {
            components.len() >= 3
                && components[0].eq_ignore_ascii_case("Templates")
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        }
    };
    if !valid {
        return Err(Error::InvalidInput {
            message: format!(
                "journal target does not match the {} descriptor boundary: {}",
                target_kind.as_str(),
                path.display(),
            ),
        });
    }
    Ok(())
}

fn inspect_recorded_file(
    file: &JournalFile,
    path: &Path,
    target_kind: OperationTargetKind,
) -> RecordedFileStatus {
    match fs::read(path) {
        Ok(bytes) => {
            let actual_sha256 = sha256_hex(&bytes);
            let state = if actual_sha256 == file.sha256_after {
                RecordedFileState::Matching
            } else {
                RecordedFileState::Modified
            };
            RecordedFileStatus {
                relative_path: file.relative_path.clone(),
                expected_sha256: file.sha256_after.clone(),
                actual_sha256: Some(actual_sha256),
                state,
                message: None,
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => RecordedFileStatus {
            relative_path: file.relative_path.clone(),
            expected_sha256: file.sha256_after.clone(),
            actual_sha256: None,
            state: RecordedFileState::Missing,
            message: Some(match target_kind {
                OperationTargetKind::Engine => {
                    "Recorded descriptor is missing. Repair the engine or restore the snapshot."
                        .to_owned()
                }
                OperationTargetKind::Project => {
                    "Recorded project is missing. Restore the project file or select another project."
                        .to_owned()
                }
                OperationTargetKind::Template => {
                    "Recorded template is missing. Repair the engine or restore the snapshot."
                        .to_owned()
                }
            }),
        },
        Err(error) => RecordedFileStatus {
            relative_path: file.relative_path.clone(),
            expected_sha256: file.sha256_after.clone(),
            actual_sha256: None,
            state: RecordedFileState::Unreadable,
            message: Some(format!(
                "Recorded descriptor read failed: {error}. Check the file permissions and retry."
            )),
        },
    }
}

pub(crate) fn paths_match(left: &Path, right: &Path) -> bool {
    path_text_matches(left, right)
}

#[cfg(windows)]
fn path_text_matches(left: &Path, right: &Path) -> bool {
    windows_path_text(left).eq_ignore_ascii_case(&windows_path_text(right))
}

#[cfg(windows)]
fn windows_path_text(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    if let Some(value) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{value}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    }
}

#[cfg(not(windows))]
fn path_text_matches(left: &Path, right: &Path) -> bool {
    left == right
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    #[cfg(windows)]
    use super::paths_match;
    use super::{
        JOURNAL_SCHEMA, JournalFile, JournalOperation, JournalOperationKind, JournalState,
        OperationTargetKind, RecordedFileState, inspect_engine_status, inspect_project_status,
    };
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use crate::plans::sha256_hex;

    #[cfg(windows)]
    #[test]
    fn windows_path_identity_accepts_verbatim_prefixes() {
        assert!(paths_match(
            Path::new(r"C:\Build\UE_Invented"),
            Path::new(r"\\?\C:\Build\UE_Invented")
        ));
        assert!(paths_match(
            Path::new(r"\\server\share\UE_Invented"),
            Path::new(r"\\?\UNC\server\share\UE_Invented")
        ));
    }

    #[test]
    fn status_reports_matching_modified_and_missing_targets() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_Invented");
        let matching = write_plugin(
            &engine_path,
            "Runtime/Matching/Matching.uplugin",
            b"matching",
        )?;
        let modified = write_plugin(
            &engine_path,
            "Runtime/Modified/Modified.uplugin",
            b"modified",
        )?;
        let missing_relative = "Engine/Plugins/Runtime/Missing/Missing.uplugin";
        let state_path = temp.path().join("state.toml");
        let source = journal_source(
            &engine_path,
            temp.path(),
            &[
                (
                    "Engine/Plugins/Runtime/Matching/Matching.uplugin",
                    &sha256_hex(b"matching"),
                ),
                (
                    "Engine/Plugins/Runtime/Modified/Modified.uplugin",
                    &sha256_hex(b"before"),
                ),
                (missing_relative, &sha256_hex(b"missing")),
            ],
        );
        fs::write(&state_path, source)?;

        let status = inspect_engine_status(&engine(&engine_path), &state_path)?;

        assert!(status.recorded);
        assert!(status.drifted);
        assert_eq!(status.files[0].state, RecordedFileState::Matching);
        assert_eq!(status.files[1].state, RecordedFileState::Modified);
        assert_eq!(status.files[2].state, RecordedFileState::Missing);
        assert_eq!(status.files[0].actual_sha256, Some(sha256_hex(b"matching")));
        assert!(matching.exists());
        assert!(modified.exists());
        Ok(())
    }

    #[test]
    fn missing_journal_reports_no_recorded_operation() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_Invented");
        fs::create_dir_all(&engine_path)?;

        let status = inspect_engine_status(
            &engine(&engine_path),
            &temp.path().join("missing-state.toml"),
        )?;

        assert!(!status.recorded);
        assert!(!status.drifted);
        assert!(status.files.is_empty());
        Ok(())
    }

    #[test]
    fn journal_rejects_paths_that_escape_the_engine() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let source = format!(
            "schema = 1\n[[operation]]\nid = \"test\"\nengine_path = \"{}\"\npreset = \"Test\"\ncompleted = \"test-time\"\nbackup_directory = \"{}\"\n[[operation.file]]\nrelative = \"../outside.uplugin\"\nsha256_after = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
            toml_path(&temp.path().join("UE")),
            toml_path(&temp.path().join("backup"))
        );

        let error = JournalState::parse(&source).err();

        assert!(error.is_some_and(|value| value.to_string().contains("safe engine-relative")));
        Ok(())
    }

    #[test]
    fn project_journal_round_trip_and_status_track_one_project() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let project_path = temp.path().join("Fixture.uproject");
        fs::write(&project_path, b"{\"FileVersion\":3}")?;
        let state = JournalState {
            schema: JOURNAL_SCHEMA,
            operations: vec![JournalOperation {
                id: "project-operation".to_owned(),
                kind: JournalOperationKind::Apply,
                target_kind: OperationTargetKind::Project,
                engine_path: temp.path().join("UE_5.8"),
                engine_version: Some("5.8.0".to_owned()),
                project_path: Some(project_path.clone()),
                preset: "Manual project edit".to_owned(),
                completed: "test-time".to_owned(),
                backup_directory: temp.path().join("backups"),
                source_snapshot: None,
                files: vec![JournalFile {
                    relative_path: PathBuf::from("Fixture.uproject"),
                    sha256_after: sha256_hex(b"{\"FileVersion\":3}"),
                }],
            }],
        };
        let state_path = temp.path().join("state.toml");
        let rendered = state.render();
        fs::write(&state_path, &rendered)?;

        assert_eq!(JournalState::parse(&rendered)?, state);
        let matching = inspect_project_status(&project_path, &state_path)?;
        assert!(matching.recorded);
        assert!(!matching.drifted);
        assert_eq!(matching.files[0].state, RecordedFileState::Matching);

        fs::write(&project_path, b"{\"FileVersion\":4}")?;
        let modified = inspect_project_status(&project_path, &state_path)?;
        assert!(modified.drifted);
        assert_eq!(modified.files[0].state, RecordedFileState::Modified);
        Ok(())
    }

    fn write_plugin(
        engine: &Path,
        relative: &str,
        bytes: &[u8],
    ) -> Result<std::path::PathBuf, Box<dyn StdError>> {
        let path = engine.join("Engine").join("Plugins").join(relative);
        let parent = path.parent().ok_or("plugin fixture has no parent")?;
        fs::create_dir_all(parent)?;
        fs::write(&path, bytes)?;
        Ok(path)
    }

    fn journal_source(engine: &Path, backup: &Path, files: &[(&str, &str)]) -> String {
        let mut source = format!(
            "schema = 1\n[[operation]]\nid = \"test-operation\"\nengine_path = \"{}\"\nengine_version = \"5.9.0\"\npreset = \"Invented\"\ncompleted = \"test-time\"\nbackup_directory = \"{}\"\n",
            toml_path(engine),
            toml_path(backup)
        );
        for (relative, hash) in files {
            source.push_str("[[operation.file]]\nrelative = \"");
            source.push_str(relative);
            source.push_str("\"\nsha256_after = \"");
            source.push_str(hash);
            source.push_str("\"\n");
        }
        source
    }

    fn toml_path(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    fn engine(path: &Path) -> EngineInstallation {
        EngineInstallation {
            path: path.to_path_buf(),
            version: Some("5.9.0".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Partial,
            descriptor_count: 0,
            issues: Vec::new(),
        }
    }
}
