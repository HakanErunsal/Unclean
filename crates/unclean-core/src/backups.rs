//! Defines recovery manifests and validates snapshot metadata before restore planning.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::descriptors::DeclaredPluginState;
use crate::journal::{OperationTargetKind, valid_sha256, validate_relative_target_path};
use crate::{Error, Result};

/// Identifies the backup manifest schema accepted by this build.
pub const BACKUP_MANIFEST_SCHEMA: i64 = 1;

/// Identifies the operation that created one recovery snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupOperationKind {
    /// Records bytes saved before a preset apply.
    Apply,
    /// Records bytes saved before a snapshot restore.
    Restore,
}

impl BackupOperationKind {
    /// Returns the stable lowercase label used in manifest TOML.
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
                message: format!("backup manifest schema 1 does not support operation \"{value}\""),
            }),
        }
    }
}

/// Records the complete recovery source for one prepared engine or project transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupManifest {
    /// Identifies the manifest schema.
    pub schema: i64,
    /// Identifies the operation that owns the backup directory.
    pub operation_id: String,
    /// Identifies apply or restore preparation.
    pub operation: BackupOperationKind,
    /// Identifies the descriptor boundary protected by the snapshot.
    pub target_kind: OperationTargetKind,
    /// Records the canonical engine path.
    pub engine_path: PathBuf,
    /// Records the engine version when discovery supplied one.
    pub engine_version: Option<String>,
    /// Records the selected project descriptor for project operations.
    pub project_path: Option<PathBuf>,
    /// Records the operation creation time.
    pub created: String,
    /// Names the preset associated with the operation.
    pub preset: String,
    /// Records the resolved preset file path.
    pub preset_path: Option<PathBuf>,
    /// Identifies the restored snapshot when this manifest protects a restore.
    pub source_snapshot: Option<String>,
    /// Lists every target prepared before the first replacement.
    pub files: Vec<BackupManifestFile>,
}

impl BackupManifest {
    /// Renders schema 1 TOML with escaped paths and strings.
    #[must_use]
    pub fn render(&self) -> String {
        let mut document = DocumentMut::new();
        document["schema"] = value(self.schema);
        document["operation_id"] = value(self.operation_id.as_str());
        document["operation"] = value(self.operation.as_str());
        if self.target_kind != OperationTargetKind::Engine {
            document["target_kind"] = value(self.target_kind.as_str());
        }
        document["engine_path"] = value(self.engine_path.to_string_lossy().as_ref());
        if let Some(version) = &self.engine_version {
            document["engine_version"] = value(version.as_str());
        }
        if let Some(project_path) = &self.project_path {
            document["project_path"] = value(project_path.to_string_lossy().as_ref());
        }
        document["created"] = value(self.created.as_str());
        document["preset"] = value(self.preset.as_str());
        if let Some(preset_path) = &self.preset_path {
            document["preset_path"] = value(preset_path.to_string_lossy().as_ref());
        }
        if let Some(snapshot) = &self.source_snapshot {
            document["source_snapshot"] = value(snapshot.as_str());
        }
        let mut files = ArrayOfTables::new();
        for file in &self.files {
            files.push(file_table(file));
        }
        document["file"] = Item::ArrayOfTables(files);
        document.to_string()
    }

    /// Parses and validates one recovery manifest.
    ///
    /// # Errors
    ///
    /// Returns an error when TOML, paths, hashes, states, lengths, or required fields are invalid.
    pub fn parse(source: &str) -> Result<Self> {
        let document = source
            .parse::<DocumentMut>()
            .map_err(|error| Error::InvalidInput {
                message: format!("backup manifest TOML is invalid: {error}"),
            })?;
        reject_document_fields(&document)?;
        let schema = required_document_integer(&document, "schema")?;
        if schema != BACKUP_MANIFEST_SCHEMA {
            return Err(Error::InvalidInput {
                message: format!(
                    "this build supports backup manifest schema {BACKUP_MANIFEST_SCHEMA}; file uses schema {schema}"
                ),
            });
        }
        let operation_id = required_document_string(&document, "operation_id")?;
        if !valid_identifier(&operation_id) {
            return Err(Error::InvalidInput {
                message: "backup manifest operation identifier is invalid".to_owned(),
            });
        }
        let engine_path = PathBuf::from(required_document_string(&document, "engine_path")?);
        if !engine_path.is_absolute() {
            return Err(Error::InvalidInput {
                message: "backup manifest engine path must be absolute".to_owned(),
            });
        }
        let target_kind = optional_document_string(&document, "target_kind")?
            .map(|value| match value.as_str() {
                "engine" => Ok(OperationTargetKind::Engine),
                "project" => Ok(OperationTargetKind::Project),
                "template" => Ok(OperationTargetKind::Template),
                _ => Err(Error::InvalidInput {
                    message: format!(
                        "backup manifest schema 1 does not support target kind \"{value}\""
                    ),
                }),
            })
            .transpose()?
            .unwrap_or(OperationTargetKind::Engine);
        let project_path = optional_document_string(&document, "project_path")?.map(PathBuf::from);
        match (target_kind, &project_path) {
            (OperationTargetKind::Engine | OperationTargetKind::Template, None) => {}
            (OperationTargetKind::Project, Some(path)) if path.is_absolute() => {}
            (OperationTargetKind::Engine | OperationTargetKind::Template, Some(_)) => {
                return Err(Error::InvalidInput {
                    message: "backup manifest records a project path for a non-project target"
                        .to_owned(),
                });
            }
            (OperationTargetKind::Project, _) => {
                return Err(Error::InvalidInput {
                    message: "backup manifest requires an absolute project path".to_owned(),
                });
            }
        }
        let files = document
            .get("file")
            .and_then(Item::as_array_of_tables)
            .ok_or_else(|| Error::InvalidInput {
                message: "backup manifest field \"file\" must contain tables".to_owned(),
            })?
            .iter()
            .enumerate()
            .map(|(index, table)| parse_file(table, index, target_kind))
            .collect::<Result<Vec<_>>>()?;
        let mut relative_paths = HashSet::with_capacity(files.len());
        if files
            .iter()
            .any(|file| !relative_paths.insert(file.relative_path.clone()))
        {
            return Err(Error::InvalidInput {
                message: "backup manifest contains a duplicate target path".to_owned(),
            });
        }
        let preset_path = optional_document_string(&document, "preset_path")?.map(PathBuf::from);
        if preset_path.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err(Error::InvalidInput {
                message: "backup manifest preset path must be absolute".to_owned(),
            });
        }
        Ok(Self {
            schema,
            operation_id,
            operation: BackupOperationKind::parse(&required_document_string(
                &document,
                "operation",
            )?)?,
            target_kind,
            engine_path,
            engine_version: optional_document_string(&document, "engine_version")?,
            project_path,
            created: required_document_string(&document, "created")?,
            preset: required_document_string(&document, "preset")?,
            preset_path,
            source_snapshot: optional_document_string(&document, "source_snapshot")?,
            files,
        })
    }
}

/// Records hashes, semantic states, and metadata for one backed-up target.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BackupManifestFile {
    /// Records the target path relative to its engine or project boundary.
    pub relative_path: PathBuf,
    /// Reports whether the target existed before the transaction.
    pub source_existed: bool,
    /// Records the source hash when the target existed.
    pub sha256_before: Option<String>,
    /// Records the planned and verified post-write hash.
    pub sha256_after: String,
    /// Records the source state when the source parsed as a plugin descriptor.
    pub value_before: Option<DeclaredPluginState>,
    /// Records the required post-write state.
    pub value_after: Option<DeclaredPluginState>,
    /// Records the source byte count when the target existed.
    pub source_length: Option<u64>,
    /// Records the planned byte count.
    pub planned_length: u64,
    /// Records the source read-only attribute.
    pub readonly_before: bool,
}

/// Loads and validates `manifest.toml` from one backup directory.
///
/// # Errors
///
/// Returns an error when the manifest is missing, unreadable, or invalid.
pub fn load_backup_manifest(backup_directory: &Path) -> Result<BackupManifest> {
    let path = backup_directory.join("manifest.toml");
    let source = fs::read_to_string(&path).map_err(|error| match error.kind() {
        ErrorKind::NotFound => Error::NotFound {
            item: format!("backup manifest {}", path.display()),
        },
        ErrorKind::PermissionDenied => Error::PermissionDenied {
            message: format!("Unclean cannot read backup manifest {}", path.display()),
        },
        _ => Error::Internal {
            message: format!("backup manifest read failed: {error}"),
        },
    })?;
    BackupManifest::parse(&source)
}

fn file_table(file: &BackupManifestFile) -> Table {
    let mut table = Table::new();
    table["relative"] = value(file.relative_path.to_string_lossy().as_ref());
    table["source_existed"] = value(file.source_existed);
    if let Some(hash) = &file.sha256_before {
        table["sha256_before"] = value(hash.as_str());
    }
    table["sha256_after"] = value(file.sha256_after.as_str());
    if let Some(state) = file.value_before {
        table["value_before"] = value(state.as_str());
    }
    if let Some(state) = file.value_after {
        table["value_after"] = value(state.as_str());
    }
    if let Some(length) = file.source_length {
        table["source_length"] = value(i64::try_from(length).unwrap_or(i64::MAX));
    }
    table["planned_length"] = value(i64::try_from(file.planned_length).unwrap_or(i64::MAX));
    table["readonly_before"] = value(file.readonly_before);
    table
}

fn parse_file(
    table: &Table,
    index: usize,
    target_kind: OperationTargetKind,
) -> Result<BackupManifestFile> {
    reject_table_fields(
        table,
        &[
            "relative",
            "source_existed",
            "sha256_before",
            "sha256_after",
            "value_before",
            "value_after",
            "source_length",
            "planned_length",
            "readonly_before",
        ],
        index,
    )?;
    let relative_path = PathBuf::from(required_table_string(table, "relative", index)?);
    validate_relative_target_path(target_kind, &relative_path)?;
    let source_existed = required_table_bool(table, "source_existed", index)?;
    let sha256_before = optional_table_string(table, "sha256_before", index)?;
    if source_existed && sha256_before.is_none() {
        return Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} requires sha256_before for an existing source"
            ),
        });
    }
    if sha256_before
        .as_ref()
        .is_some_and(|hash| !valid_sha256(hash))
    {
        return Err(Error::InvalidInput {
            message: format!("backup manifest file {index} has an invalid source SHA-256 digest"),
        });
    }
    let sha256_after = required_table_string(table, "sha256_after", index)?;
    if !valid_sha256(&sha256_after) {
        return Err(Error::InvalidInput {
            message: format!("backup manifest file {index} has an invalid planned SHA-256 digest"),
        });
    }
    let source_length = optional_table_u64(table, "source_length", index)?;
    if source_existed && source_length.is_none() {
        return Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} requires source_length for an existing source"
            ),
        });
    }
    let value_before = optional_table_state(table, "value_before", index)?;
    let value_after = optional_table_state(table, "value_after", index)?;
    if target_kind == OperationTargetKind::Engine && source_existed && value_before.is_none() {
        return Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} requires value_before for an existing source"
            ),
        });
    }
    if target_kind == OperationTargetKind::Engine && value_after.is_none() {
        return Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} requires value_after for an engine target"
            ),
        });
    }
    if target_kind != OperationTargetKind::Engine
        && (value_before.is_some() || value_after.is_some())
    {
        return Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} records engine state for a non-engine target"
            ),
        });
    }
    if !source_existed
        && (sha256_before.is_some() || source_length.is_some() || value_before.is_some())
    {
        return Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} records source metadata for an absent source"
            ),
        });
    }
    Ok(BackupManifestFile {
        relative_path,
        source_existed,
        sha256_before,
        sha256_after,
        value_before,
        value_after,
        source_length,
        planned_length: required_table_u64(table, "planned_length", index)?,
        readonly_before: required_table_bool(table, "readonly_before", index)?,
    })
}

fn reject_document_fields(document: &DocumentMut) -> Result<()> {
    const ACCEPTED: [&str; 12] = [
        "schema",
        "operation_id",
        "operation",
        "target_kind",
        "engine_path",
        "engine_version",
        "project_path",
        "created",
        "preset",
        "preset_path",
        "source_snapshot",
        "file",
    ];
    for (key, _) in document.iter() {
        if !ACCEPTED.contains(&key) {
            return Err(Error::InvalidInput {
                message: format!("backup manifest field \"{key}\" is not supported by schema 1"),
            });
        }
    }
    Ok(())
}

fn reject_table_fields(table: &Table, accepted: &[&str], index: usize) -> Result<()> {
    for (key, _) in table {
        if !accepted.contains(&key) {
            return Err(Error::InvalidInput {
                message: format!(
                    "backup manifest file {index} field \"{key}\" is not supported by schema 1"
                ),
            });
        }
    }
    Ok(())
}

fn required_document_string(document: &DocumentMut, key: &str) -> Result<String> {
    document
        .get(key)
        .and_then(Item::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput {
            message: format!("backup manifest field \"{key}\" must contain a nonempty string"),
        })
}

fn optional_document_string(document: &DocumentMut, key: &str) -> Result<Option<String>> {
    document
        .get(key)
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidInput {
                    message: format!("backup manifest field \"{key}\" must contain a string"),
                })
        })
        .transpose()
}

fn required_document_integer(document: &DocumentMut, key: &str) -> Result<i64> {
    document
        .get(key)
        .and_then(Item::as_integer)
        .ok_or_else(|| Error::InvalidInput {
            message: format!("backup manifest field \"{key}\" must contain an integer"),
        })
}

fn required_table_string(table: &Table, key: &str, index: usize) -> Result<String> {
    table
        .get(key)
        .and_then(Item::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput {
            message: format!(
                "backup manifest file {index} field \"{key}\" must contain a nonempty string"
            ),
        })
}

fn optional_table_string(table: &Table, key: &str, index: usize) -> Result<Option<String>> {
    table
        .get(key)
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::InvalidInput {
                    message: format!(
                        "backup manifest file {index} field \"{key}\" must contain a string"
                    ),
                })
        })
        .transpose()
}

fn required_table_bool(table: &Table, key: &str, index: usize) -> Result<bool> {
    table
        .get(key)
        .and_then(Item::as_bool)
        .ok_or_else(|| Error::InvalidInput {
            message: format!("backup manifest file {index} field \"{key}\" must contain a Boolean"),
        })
}

fn required_table_u64(table: &Table, key: &str, index: usize) -> Result<u64> {
    table
        .get(key)
        .and_then(Item::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| Error::InvalidInput {
            message: format!(
                "backup manifest file {index} field \"{key}\" must contain a nonnegative integer"
            ),
        })
}

fn optional_table_u64(table: &Table, key: &str, index: usize) -> Result<Option<u64>> {
    table
        .get(key)
        .map(|item| {
            item.as_integer()
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(|| Error::InvalidInput {
                    message: format!(
                        "backup manifest file {index} field \"{key}\" must contain a nonnegative integer"
                    ),
                })
        })
        .transpose()
}

fn optional_table_state(
    table: &Table,
    key: &str,
    index: usize,
) -> Result<Option<DeclaredPluginState>> {
    optional_table_string(table, key, index)?
        .map(|value| parse_state(&value, index, key))
        .transpose()
}

fn parse_state(value: &str, index: usize, key: &str) -> Result<DeclaredPluginState> {
    match value {
        "enabled" => Ok(DeclaredPluginState::Enabled),
        "disabled" => Ok(DeclaredPluginState::Disabled),
        "unspecified" => Ok(DeclaredPluginState::Unspecified),
        _ => Err(Error::InvalidInput {
            message: format!(
                "backup manifest file {index} field \"{key}\" has unsupported state \"{value}\""
            ),
        }),
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{BACKUP_MANIFEST_SCHEMA, BackupManifest, BackupManifestFile, BackupOperationKind};
    use crate::descriptors::DeclaredPluginState;
    use crate::journal::OperationTargetKind;

    #[test]
    fn manifest_round_trip_retains_recovery_fields() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let manifest = BackupManifest {
            schema: BACKUP_MANIFEST_SCHEMA,
            operation_id: "test-operation".to_owned(),
            operation: BackupOperationKind::Apply,
            target_kind: OperationTargetKind::Engine,
            engine_path: temp.path().join("UE"),
            engine_version: Some("5.9.0".to_owned()),
            project_path: None,
            created: "test-time".to_owned(),
            preset: "Invented".to_owned(),
            preset_path: Some(temp.path().join("presets").join("invented.toml")),
            source_snapshot: None,
            files: vec![BackupManifestFile {
                relative_path: PathBuf::from("Engine/Plugins/Runtime/Invented/Invented.uplugin"),
                source_existed: true,
                sha256_before: Some(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                ),
                sha256_after: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
                value_before: Some(DeclaredPluginState::Enabled),
                value_after: Some(DeclaredPluginState::Disabled),
                source_length: Some(120),
                planned_length: 121,
                readonly_before: true,
            }],
        };

        let parsed = BackupManifest::parse(&manifest.render())?;

        assert_eq!(parsed, manifest);
        Ok(())
    }

    #[test]
    fn manifest_rejects_an_escaping_target() -> Result<(), Box<dyn StdError>> {
        let temp = tempdir()?;
        let source = format!(
            concat!(
                "schema = 1\n",
                "operation_id = \"test\"\n",
                "operation = \"apply\"\n",
                "engine_path = \"{}\"\n",
                "created = \"test-time\"\n",
                "preset = \"Invented\"\n",
                "preset_path = \"{}\"\n",
                "[[file]]\n",
                "relative = \"../outside.uplugin\"\n",
                "source_existed = true\n",
                "sha256_before = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
                "sha256_after = \"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"\n",
                "value_before = \"enabled\"\n",
                "value_after = \"disabled\"\n",
                "source_length = 10\n",
                "planned_length = 11\n",
                "readonly_before = false\n",
            ),
            toml_path(&temp.path().join("UE")),
            toml_path(&temp.path().join("preset.toml"))
        );

        let error = BackupManifest::parse(&source)
            .err()
            .ok_or("escaping path was accepted")?;

        assert!(error.to_string().contains("safe engine-relative"));
        Ok(())
    }

    #[test]
    fn project_manifest_round_trip_omits_engine_descriptor_states() -> Result<(), Box<dyn StdError>>
    {
        let temp = tempdir()?;
        let manifest = BackupManifest {
            schema: BACKUP_MANIFEST_SCHEMA,
            operation_id: "project-operation".to_owned(),
            operation: BackupOperationKind::Apply,
            target_kind: OperationTargetKind::Project,
            engine_path: temp.path().join("UE_5.8"),
            engine_version: Some("5.8.0".to_owned()),
            project_path: Some(temp.path().join("Fixture.uproject")),
            created: "test-time".to_owned(),
            preset: "Manual project edit".to_owned(),
            preset_path: None,
            source_snapshot: None,
            files: vec![BackupManifestFile {
                relative_path: PathBuf::from("Fixture.uproject"),
                source_existed: true,
                sha256_before: Some(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                ),
                sha256_after: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
                value_before: None,
                value_after: None,
                source_length: Some(120),
                planned_length: 121,
                readonly_before: false,
            }],
        };

        let rendered = manifest.render();
        let parsed = BackupManifest::parse(&rendered)?;

        assert_eq!(parsed, manifest);
        assert!(rendered.contains("target_kind = \"project\""));
        assert!(!rendered.contains("value_before"));
        assert!(!rendered.contains("value_after"));
        Ok(())
    }

    fn toml_path(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }
}
