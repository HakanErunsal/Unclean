//! Discovers Unreal project templates and builds focused suppression plans.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::discovery::EngineInstallation;
use crate::plans::{
    PLAN_SCHEMA, PlanBuildOptions, PlannedByteChange, differing_ranges, sha256_hex,
};
use crate::projects::{
    ProjectDescriptorDocument, ProjectDescriptorEdit, ProjectSuppressionEdit,
    ProjectSuppressionState,
};
use crate::{Error, Result};

/// Identifies a template scan problem without hiding valid templates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateScanWarningCode {
    /// Reports a directory or file access failure.
    ScanFailed,
    /// Reports template descriptor parse failures.
    DescriptorInvalid,
}

impl TemplateScanWarningCode {
    /// Returns the stable lowercase warning code used by frontends.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScanFailed => "scan_failed",
            Self::DescriptorInvalid => "descriptor_invalid",
        }
    }
}

/// Reports one template scan problem with a recovery action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TemplateScanWarning {
    /// Identifies the stable warning category.
    pub code: TemplateScanWarningCode,
    /// Records the path that caused a scan or parse failure.
    pub path: PathBuf,
    /// Explains the failure and the next action.
    pub message: String,
}

/// Describes one valid Unreal project template.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EngineTemplate {
    /// Names the template from its descriptor file.
    pub name: String,
    /// Records the canonical template descriptor path.
    pub path: PathBuf,
    /// Records the descriptor path relative to the engine root.
    pub relative_path: PathBuf,
    /// Reports the current engine-plugin suppression state.
    pub suppression: ProjectSuppressionState,
    /// Counts explicit plugin references retained by the template.
    pub plugin_reference_count: usize,
}

/// Lists valid templates and nonfatal scan findings for one engine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TemplateCatalog {
    /// Records the selected engine.
    pub engine: EngineInstallation,
    /// Lists valid templates in stable path order.
    pub templates: Vec<EngineTemplate>,
    /// Lists directory and descriptor failures.
    pub warnings: Vec<TemplateScanWarning>,
}

/// Describes one selected template before and after a suppression plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedTemplate {
    /// Names the selected template.
    pub name: String,
    /// Records the descriptor path relative to the engine root.
    pub relative_path: PathBuf,
    /// Reports the current suppression state.
    pub suppression_before: ProjectSuppressionState,
    /// Reports the planned suppression state.
    pub suppression_after: ProjectSuppressionState,
    /// Counts explicit plugin references that remain unchanged.
    pub plugin_reference_count: usize,
    /// Reports whether the descriptor bytes need to change.
    pub changed: bool,
}

/// Stores one exact template descriptor edit for the protected writer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedTemplateFileEdit {
    /// Names the selected template.
    pub template: String,
    /// Records the canonical target path.
    pub path: PathBuf,
    /// Records the descriptor path relative to the engine root.
    pub relative_path: PathBuf,
    /// Reports the current suppression state.
    pub suppression_before: ProjectSuppressionState,
    /// Reports the planned suppression state.
    pub suppression_after: ProjectSuppressionState,
    /// Hashes the exact source bytes read during planning.
    pub sha256_before: String,
    /// Hashes the exact verified planned bytes.
    pub sha256_after: String,
    /// Reports the planned output size without serializing descriptor content.
    pub planned_byte_count: usize,
    /// Locates the differing ranges in both byte streams.
    pub byte_change: PlannedByteChange,
    #[serde(skip)]
    planned_bytes: Vec<u8>,
}

impl PlannedTemplateFileEdit {
    /// Returns the verified output bytes retained by the immutable plan.
    #[must_use]
    pub fn planned_bytes(&self) -> &[u8] {
        &self.planned_bytes
    }
}

/// Owns one reviewed template suppression plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TemplatePlan {
    schema: u8,
    operation_id: String,
    engine: EngineInstallation,
    suppression: ProjectSuppressionEdit,
    backup_directory: PathBuf,
    templates: Vec<PlannedTemplate>,
    changes: Vec<PlannedTemplateFileEdit>,
    warnings: Vec<TemplateScanWarning>,
}

impl TemplatePlan {
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

    /// Returns the selected engine.
    #[must_use]
    pub const fn engine(&self) -> &EngineInstallation {
        &self.engine
    }

    /// Returns the requested suppression edit.
    #[must_use]
    pub const fn suppression(&self) -> ProjectSuppressionEdit {
        self.suppression
    }

    /// Returns the exact backup directory reserved for this plan.
    #[must_use]
    pub fn backup_directory(&self) -> &Path {
        &self.backup_directory
    }

    /// Returns every selected template, including no-op entries.
    #[must_use]
    pub fn templates(&self) -> &[PlannedTemplate] {
        &self.templates
    }

    /// Returns verified descriptor edits in stable path order.
    #[must_use]
    pub fn changes(&self) -> &[PlannedTemplateFileEdit] {
        &self.changes
    }

    /// Returns nonfatal findings from template discovery.
    #[must_use]
    pub fn warnings(&self) -> &[TemplateScanWarning] {
        &self.warnings
    }
}

/// Scans valid `.uproject` files below one engine `Templates` directory.
///
/// # Errors
///
/// Returns an error when the template root is missing or path resolution fails.
pub fn scan_engine_templates(engine: &EngineInstallation) -> Result<TemplateCatalog> {
    let engine_root = fs::canonicalize(&engine.path).map_err(|error| Error::NotFound {
        item: format!(
            "engine root {}: {error}. Refresh engine discovery and retry.",
            engine.path.display()
        ),
    })?;
    let template_root = engine.path.join("Templates");
    let resolved_root = fs::canonicalize(&template_root).map_err(|error| match error.kind() {
        ErrorKind::NotFound => Error::NotFound {
            item: format!(
                "template directory {}. Repair the engine installation and retry.",
                template_root.display()
            ),
        },
        ErrorKind::PermissionDenied => Error::PermissionDenied {
            message: format!(
                "Template directory read failed at {}. Check its permissions and retry.",
                template_root.display()
            ),
        },
        _ => Error::Internal {
            message: format!(
                "Template directory resolution failed at {}: {error}. Check the installation and retry.",
                template_root.display()
            ),
        },
    })?;
    let mut paths = Vec::new();
    let mut warnings = Vec::new();
    collect_template_paths(&resolved_root, &mut paths, &mut warnings);
    paths.sort_by_key(|path| path_identity(path));

    let mut templates = Vec::with_capacity(paths.len());
    for path in paths {
        match read_template(&engine.path, &engine_root, &path) {
            Ok(template) => templates.push(template),
            Err(error) => warnings.push(TemplateScanWarning {
                code: TemplateScanWarningCode::DescriptorInvalid,
                path,
                message: format!(
                    "Template descriptor load failed: {error}. Repair the descriptor or exclude this template."
                ),
            }),
        }
    }
    Ok(TemplateCatalog {
        engine: engine.clone(),
        templates,
        warnings,
    })
}

/// Resolves exact template names or engine-relative paths from one catalog.
///
/// # Errors
///
/// Returns an error when a selector is empty, missing, ambiguous, or repeated.
pub fn resolve_template_selection(
    catalog: &TemplateCatalog,
    selectors: &[String],
) -> Result<Vec<PathBuf>> {
    if selectors.is_empty() {
        return Err(Error::InvalidInput {
            message: "Select at least one template or use --all.".to_owned(),
        });
    }
    let mut names: HashMap<String, Vec<&EngineTemplate>> = HashMap::new();
    let mut paths = HashMap::new();
    for template in &catalog.templates {
        names
            .entry(template.name.to_ascii_lowercase())
            .or_default()
            .push(template);
        paths.insert(path_identity(&template.relative_path), template);
    }

    let mut selected = Vec::with_capacity(selectors.len());
    let mut seen = HashSet::with_capacity(selectors.len());
    for selector in selectors {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(Error::InvalidInput {
                message: "Template selector is empty. Provide a template name or relative path."
                    .to_owned(),
            });
        }
        let path_key = path_identity(Path::new(selector));
        let template = if let Some(template) = paths.get(&path_key) {
            *template
        } else {
            let matches = names
                .get(&selector.to_ascii_lowercase())
                .map(Vec::as_slice)
                .unwrap_or_default();
            match matches {
                [template] => *template,
                [] => {
                    return Err(Error::NotFound {
                        item: format!(
                            "template \"{selector}\". Run `unclean templates` and choose a listed name or path."
                        ),
                    });
                }
                _ => {
                    return Err(Error::Conflict {
                        message: format!(
                            "Template name \"{selector}\" is ambiguous. Use an engine-relative path."
                        ),
                    });
                }
            }
        };
        let identity = path_identity(&template.relative_path);
        if !seen.insert(identity) {
            return Err(Error::InvalidInput {
                message: format!(
                    "Remove the duplicate selector for template {}.",
                    template.relative_path.display()
                ),
            });
        }
        selected.push(template.relative_path.clone());
    }
    selected.sort_by_key(|path| path_identity(path));
    Ok(selected)
}

/// Builds a reviewed suppression plan for explicitly selected templates.
///
/// # Errors
///
/// Returns an error when the selection leaves the template boundary or focused editing fails.
pub fn build_template_plan(
    engine: &EngineInstallation,
    selected_relative_paths: &[PathBuf],
    suppression: ProjectSuppressionEdit,
    options: &PlanBuildOptions,
) -> Result<TemplatePlan> {
    if suppression == ProjectSuppressionEdit::Keep {
        return Err(Error::InvalidInput {
            message:
                "Template suppression has no requested value. Choose enabled, disabled, or clear."
                    .to_owned(),
        });
    }
    let catalog = scan_engine_templates(engine)?;
    if selected_relative_paths.is_empty() {
        return Err(Error::InvalidInput {
            message: "Select at least one template before planning.".to_owned(),
        });
    }
    let available = catalog
        .templates
        .iter()
        .map(|template| (path_identity(&template.relative_path), template))
        .collect::<HashMap<_, _>>();
    let mut seen = HashSet::with_capacity(selected_relative_paths.len());
    let mut templates = Vec::with_capacity(selected_relative_paths.len());
    let mut changes = Vec::with_capacity(selected_relative_paths.len());
    for relative_path in selected_relative_paths {
        let identity = path_identity(relative_path);
        if !seen.insert(identity.clone()) {
            return Err(Error::InvalidInput {
                message: format!(
                    "Remove the duplicate selection for template {}.",
                    relative_path.display()
                ),
            });
        }
        let discovered = available.get(&identity).ok_or_else(|| Error::InvalidInput {
            message: format!(
                "Selected template is outside the discovered engine template set: {}. Refresh the template list and retry.",
                relative_path.display()
            ),
        })?;
        let (template, change) = plan_selected_template(discovered, suppression)?;
        templates.push(template);
        if let Some(change) = change {
            changes.push(change);
        }
    }
    templates.sort_by_key(|template| path_identity(&template.relative_path));
    changes.sort_by_key(|change| path_identity(&change.relative_path));
    Ok(TemplatePlan {
        schema: PLAN_SCHEMA,
        operation_id: options.operation_id().to_owned(),
        engine: engine.clone(),
        suppression,
        backup_directory: options.backup_directory(engine),
        templates,
        changes,
        warnings: catalog.warnings,
    })
}

fn plan_selected_template(
    discovered: &EngineTemplate,
    suppression: ProjectSuppressionEdit,
) -> Result<(PlannedTemplate, Option<PlannedTemplateFileEdit>)> {
    let source =
        fs::read(&discovered.path).map_err(|error| read_error(&discovered.path, &error))?;
    let document =
        ProjectDescriptorDocument::parse(&source).map_err(|error| Error::InvalidInput {
            message: format!(
                "Template descriptor is invalid at {}: {error}. Repair the descriptor and retry.",
                discovered.path.display()
            ),
        })?;
    let descriptor = document.project_descriptor(&discovered.path);
    let planned_bytes = document
        .edit(&ProjectDescriptorEdit {
            suppression,
            plugins: Vec::new(),
        })
        .map_err(|error| Error::InvalidInput {
            message: format!(
                "Template edit failed at {}: {error}. Repair the descriptor and retry.",
                discovered.path.display()
            ),
        })?;
    let verified =
        ProjectDescriptorDocument::parse(&planned_bytes).map_err(|error| Error::Internal {
            message: format!(
                "Planned template verification failed at {}: {error}. Report this failure before applying.",
                discovered.path.display()
            ),
        })?;
    let expected_state = suppression_state(suppression);
    if verified.project_descriptor(&discovered.path).suppression != expected_state {
        return Err(Error::Internal {
            message: format!(
                "Planned template has the wrong suppression state at {}. Report this failure before applying.",
                discovered.path.display()
            ),
        });
    }
    let needs_write = planned_bytes != source;
    let template = PlannedTemplate {
        name: discovered.name.clone(),
        relative_path: discovered.relative_path.clone(),
        suppression_before: descriptor.suppression,
        suppression_after: expected_state,
        plugin_reference_count: descriptor.plugins.len(),
        changed: needs_write,
    };
    let change = needs_write.then(|| PlannedTemplateFileEdit {
        template: discovered.name.clone(),
        path: discovered.path.clone(),
        relative_path: discovered.relative_path.clone(),
        suppression_before: descriptor.suppression,
        suppression_after: expected_state,
        sha256_before: sha256_hex(&source),
        sha256_after: sha256_hex(&planned_bytes),
        planned_byte_count: planned_bytes.len(),
        byte_change: differing_ranges(&source, &planned_bytes),
        planned_bytes,
    });
    Ok((template, change))
}

fn read_template(engine_path: &Path, engine_root: &Path, path: &Path) -> Result<EngineTemplate> {
    let canonical_path = fs::canonicalize(path).map_err(|error| read_error(path, &error))?;
    let relative_path =
        canonical_path
            .strip_prefix(engine_root)
            .map(Path::to_path_buf)
            .map_err(|_| Error::InvalidInput {
                message: format!(
                    "Template descriptor leaves the selected engine: {}. Repair the template path and retry.",
                    canonical_path.display()
                ),
            })?;
    if !is_template_relative_path(&relative_path) {
        return Err(Error::InvalidInput {
            message: format!(
                "Template descriptor leaves the Templates boundary: {}. Select a descriptor under the engine Templates directory.",
                relative_path.display()
            ),
        });
    }
    let bytes = fs::read(&canonical_path).map_err(|error| read_error(&canonical_path, &error))?;
    let document =
        ProjectDescriptorDocument::parse(&bytes).map_err(|error| Error::InvalidInput {
            message: format!(
                "Template descriptor is invalid at {}: {error}. Repair the descriptor and retry.",
                canonical_path.display()
            ),
        })?;
    let descriptor = document.project_descriptor(&canonical_path);
    let name = canonical_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::InvalidInput {
            message: format!(
                "Template descriptor has no valid file name at {}. Rename the descriptor and retry.",
                canonical_path.display()
            ),
        })?
        .to_owned();
    Ok(EngineTemplate {
        name,
        path: engine_path.join(&relative_path),
        relative_path,
        suppression: descriptor.suppression,
        plugin_reference_count: descriptor.plugins.len(),
    })
}

fn collect_template_paths(
    root: &Path,
    paths: &mut Vec<PathBuf>,
    warnings: &mut Vec<TemplateScanWarning>,
) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Some(entries) = read_template_directory(&directory, warnings) else {
            continue;
        };
        for entry in entries {
            let Some(entry) = read_template_entry(entry, &directory, warnings) else {
                continue;
            };
            let path = entry.path();
            let Some(file_type) = inspect_template_path(&entry, &path, warnings) else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
            {
                paths.push(path);
            }
        }
    }
}

fn read_template_directory(
    directory: &Path,
    warnings: &mut Vec<TemplateScanWarning>,
) -> Option<fs::ReadDir> {
    match fs::read_dir(directory) {
        Ok(entries) => Some(entries),
        Err(error) => {
            warnings.push(TemplateScanWarning {
                code: TemplateScanWarningCode::ScanFailed,
                path: directory.to_path_buf(),
                message: format!(
                    "Template directory scan failed: {error}. Check the directory permissions and retry."
                ),
            });
            None
        }
    }
}

fn read_template_entry(
    entry: std::io::Result<fs::DirEntry>,
    directory: &Path,
    warnings: &mut Vec<TemplateScanWarning>,
) -> Option<fs::DirEntry> {
    match entry {
        Ok(entry) => Some(entry),
        Err(error) => {
            warnings.push(TemplateScanWarning {
                code: TemplateScanWarningCode::ScanFailed,
                path: directory.to_path_buf(),
                message: format!(
                    "Template directory entry read failed: {error}. Check the directory and retry."
                ),
            });
            None
        }
    }
}

fn inspect_template_path(
    entry: &fs::DirEntry,
    path: &Path,
    warnings: &mut Vec<TemplateScanWarning>,
) -> Option<fs::FileType> {
    match entry.file_type() {
        Ok(file_type) => Some(file_type),
        Err(error) => {
            warnings.push(TemplateScanWarning {
                code: TemplateScanWarningCode::ScanFailed,
                path: path.to_path_buf(),
                message: format!(
                    "Template path inspection failed: {error}. Check the path permissions and retry."
                ),
            });
            None
        }
    }
}

fn is_template_relative_path(path: &Path) -> bool {
    let mut components = path.components();
    components
        .next()
        .is_some_and(|component| component.as_os_str().eq_ignore_ascii_case("Templates"))
        && components.next().is_some()
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
}

fn suppression_state(edit: ProjectSuppressionEdit) -> ProjectSuppressionState {
    match edit {
        ProjectSuppressionEdit::Set(true) => ProjectSuppressionState::Enabled,
        ProjectSuppressionEdit::Set(false) => ProjectSuppressionState::Disabled,
        ProjectSuppressionEdit::Clear | ProjectSuppressionEdit::Keep => {
            ProjectSuppressionState::Unspecified
        }
    }
}

fn path_identity(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn read_error(path: &Path, error: &std::io::Error) -> Error {
    match error.kind() {
        ErrorKind::NotFound => Error::NotFound {
            item: format!(
                "template descriptor {}. Refresh the template list and retry.",
                path.display()
            ),
        },
        ErrorKind::PermissionDenied => Error::PermissionDenied {
            message: format!(
                "Template descriptor read failed at {}. Check its permissions and retry.",
                path.display()
            ),
        },
        _ => Error::Internal {
            message: format!(
                "Template descriptor read failed at {}: {error}. Check the file and retry.",
                path.display()
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::discovery::{DiscoverySource, EngineHealth};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn StdError>>;

    #[test]
    fn scan_keeps_valid_templates_when_one_descriptor_fails() -> TestResult {
        let fixture = fixture()?;
        write_template(
            &fixture.engine.path,
            "TP_Blank/TP_Blank.uproject",
            br#"{"FileVersion":3,"Plugins":[{"Name":"ModelingToolsEditorMode","Enabled":true}]}"#,
        )?;
        write_template(
            &fixture.engine.path,
            "TP_Broken/TP_Broken.uproject",
            br#"{"FileVersion":"#,
        )?;

        let catalog = scan_engine_templates(&fixture.engine)?;

        assert_eq!(catalog.templates.len(), 1);
        assert_eq!(catalog.templates[0].name, "TP_Blank");
        assert_eq!(catalog.templates[0].plugin_reference_count, 1);
        assert_eq!(catalog.warnings.len(), 1);
        assert_eq!(
            catalog.warnings[0].code,
            TemplateScanWarningCode::DescriptorInvalid
        );
        Ok(())
    }

    #[test]
    fn selection_accepts_names_and_paths_and_rejects_duplicates() -> TestResult {
        let fixture = fixture()?;
        write_template(
            &fixture.engine.path,
            "TP_Blank/TP_Blank.uproject",
            br#"{"FileVersion":3}"#,
        )?;
        let catalog = scan_engine_templates(&fixture.engine)?;

        let selected = resolve_template_selection(&catalog, &["TP_Blank".to_owned()])?;
        assert_eq!(
            selected,
            vec![PathBuf::from("Templates/TP_Blank/TP_Blank.uproject")]
        );
        assert!(
            resolve_template_selection(
                &catalog,
                &[
                    "TP_Blank".to_owned(),
                    "Templates/TP_Blank/TP_Blank.uproject".to_owned()
                ]
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn plan_changes_only_suppression_for_selected_templates() -> TestResult {
        let fixture = fixture()?;
        let source = b"{\r\n\t\"FileVersion\": 3,\r\n\t\"Plugins\": [{\"Name\":\"KeepMe\",\"Enabled\":true}],\r\n}\r\n";
        write_template(&fixture.engine.path, "TP_Blank/TP_Blank.uproject", source)?;
        write_template(
            &fixture.engine.path,
            "TP_Other/TP_Other.uproject",
            br#"{"FileVersion":3}"#,
        )?;
        let other_path = fixture
            .engine
            .path
            .join("Templates/TP_Other/TP_Other.uproject");
        let other_source = fs::read(&other_path)?;
        let options = PlanBuildOptions::new(
            fixture.root.path().join("backups"),
            "template-plan".to_owned(),
        )?;

        let plan = build_template_plan(
            &fixture.engine,
            &[PathBuf::from("Templates/TP_Blank/TP_Blank.uproject")],
            ProjectSuppressionEdit::Set(true),
            &options,
        )?;

        assert_eq!(plan.templates().len(), 1);
        assert_eq!(plan.changes().len(), 1);
        let planned = plan.changes()[0].planned_bytes();
        let text = String::from_utf8(planned.to_vec())?;
        assert!(text.contains("\"DisableEnginePluginsByDefault\": true"));
        assert!(text.contains("\"Name\":\"KeepMe\""));
        assert_eq!(
            fs::read(
                fixture
                    .engine
                    .path
                    .join("Templates/TP_Blank/TP_Blank.uproject")
            )?,
            source
        );
        assert_eq!(fs::read(other_path)?, other_source);
        Ok(())
    }

    struct Fixture {
        root: tempfile::TempDir,
        engine: EngineInstallation,
    }

    fn fixture() -> TestResult<Fixture> {
        let root = tempdir()?;
        let engine_path = root.path().join("UE_5.8");
        fs::create_dir_all(engine_path.join("Templates"))?;
        Ok(Fixture {
            engine: EngineInstallation {
                path: engine_path,
                version: Some("5.8.1".to_owned()),
                source: DiscoverySource::Explicit,
                health: EngineHealth::Healthy,
                descriptor_count: 0,
                issues: Vec::new(),
            },
            root,
        })
    }

    fn write_template(engine: &Path, relative: &str, bytes: &[u8]) -> TestResult {
        let path = engine.join("Templates").join(relative);
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("template path has no parent"))?;
        fs::create_dir_all(parent)?;
        fs::write(path, bytes)?;
        Ok(())
    }
}
