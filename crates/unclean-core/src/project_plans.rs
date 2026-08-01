//! Builds immutable plans for one selected project descriptor.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::dependencies::DependencyWarning;
use crate::descriptors::{PluginScanWarning, scan_engine_plugins};
use crate::discovery::EngineInstallation;
use crate::plans::{
    CountChange, PLAN_SCHEMA, PlanBuildOptions, PlannedByteChange, differing_ranges, sha256_hex,
};
use crate::presets::{Preset, PresetPatternExpansion, UnmatchedPresetRule};
use crate::project_presets::resolve_project_preset;
use crate::project_state::{ProjectPluginOrigin, ProjectStateWarning, analyze_project_workspace};
use crate::projects::{ProjectDescriptorDocument, ProjectDescriptorEdit, ProjectSuppressionEdit};
use crate::{Error, Result};

/// Identifies the preset or manual selection used to build a project plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPlanSource {
    /// Names the source in review and history output.
    pub name: String,
    /// Records the preset path when a saved preset produced the plan.
    pub path: Option<PathBuf>,
}

/// Compares project-specific plugin counts before and after one plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPlanImpact {
    /// Counts plugins enabled after project overrides and dependency closure.
    pub effective_plugins: CountChange,
    /// Counts modules declared by effective engine plugins.
    pub declared_modules: CountChange,
    /// Counts explicit entries in the project `Plugins` array.
    pub explicit_references: CountChange,
}

/// Describes one engine plugin before and after the project edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPlannedPlugin {
    /// Names the engine plugin.
    pub plugin: String,
    /// Reports whether the current engine state enables the plugin.
    pub engine_effective_enabled: bool,
    /// Records the current explicit project reference.
    pub reference_before: Option<bool>,
    /// Records the planned explicit project reference.
    pub reference_after: Option<bool>,
    /// Reports the current project-specific effective state.
    pub effective_before: bool,
    /// Reports the planned project-specific effective state.
    pub effective_after: bool,
    /// Identifies the current project-state source.
    pub origin_before: ProjectPluginOrigin,
    /// Identifies the planned project-state source.
    pub origin_after: ProjectPluginOrigin,
}

/// Stores the selected project edit and exact bytes required by the protected writer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedProjectFileEdit {
    /// Records the canonical `.uproject` path.
    pub path: PathBuf,
    /// Records the descriptor file name relative to its parent.
    pub relative_path: PathBuf,
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

impl PlannedProjectFileEdit {
    /// Returns the verified output bytes retained by the immutable plan.
    #[must_use]
    pub fn planned_bytes(&self) -> &[u8] {
        &self.planned_bytes
    }
}

/// Owns one read-only project plan rendered by both frontends.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPlan {
    schema: u8,
    operation_id: String,
    project_path: PathBuf,
    engine: EngineInstallation,
    source: ProjectPlanSource,
    backup_directory: PathBuf,
    edit: ProjectDescriptorEdit,
    impact: ProjectPlanImpact,
    plugins: Vec<ProjectPlannedPlugin>,
    change: Option<PlannedProjectFileEdit>,
    scan_warnings: Vec<PluginScanWarning>,
    dependency_warnings: Vec<DependencyWarning>,
    project_warnings: Vec<ProjectStateWarning>,
    pattern_expansions: Vec<PresetPatternExpansion>,
    unmatched_rules: Vec<UnmatchedPresetRule>,
}

impl ProjectPlan {
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

    /// Returns the canonical project descriptor selected during planning.
    #[must_use]
    pub fn project_path(&self) -> &Path {
        &self.project_path
    }

    /// Returns the engine associated with the selected project.
    #[must_use]
    pub const fn engine(&self) -> &EngineInstallation {
        &self.engine
    }

    /// Returns the preset or manual source used during planning.
    #[must_use]
    pub const fn source(&self) -> &ProjectPlanSource {
        &self.source
    }

    /// Returns the exact backup directory reserved for this plan.
    #[must_use]
    pub fn backup_directory(&self) -> &Path {
        &self.backup_directory
    }

    /// Returns the focused project descriptor edit.
    #[must_use]
    pub const fn edit(&self) -> &ProjectDescriptorEdit {
        &self.edit
    }

    /// Returns the before-and-after project impact.
    #[must_use]
    pub const fn impact(&self) -> ProjectPlanImpact {
        self.impact
    }

    /// Returns per-plugin engine and project states.
    #[must_use]
    pub fn plugins(&self) -> &[ProjectPlannedPlugin] {
        &self.plugins
    }

    /// Returns the verified project file edit when bytes need to change.
    #[must_use]
    pub const fn change(&self) -> Option<&PlannedProjectFileEdit> {
        self.change.as_ref()
    }

    /// Returns plugin scan warnings from the associated engine.
    #[must_use]
    pub fn scan_warnings(&self) -> &[PluginScanWarning] {
        &self.scan_warnings
    }

    /// Returns dependency graph warnings from the planned project state.
    #[must_use]
    pub fn dependency_warnings(&self) -> &[DependencyWarning] {
        &self.dependency_warnings
    }

    /// Returns project override conflicts from the planned state.
    #[must_use]
    pub fn project_warnings(&self) -> &[ProjectStateWarning] {
        &self.project_warnings
    }

    /// Returns every disable pattern and its resolved plugin names.
    #[must_use]
    pub fn pattern_expansions(&self) -> &[PresetPatternExpansion] {
        &self.pattern_expansions
    }

    /// Returns exact preset rules absent from the associated engine.
    #[must_use]
    pub fn unmatched_rules(&self) -> &[UnmatchedPresetRule] {
        &self.unmatched_rules
    }
}

/// Builds a project plan from one saved preset without changing any file.
///
/// # Errors
///
/// Returns an error when project loading, engine resolution, scanning, preset resolution, or focused editing fails.
pub fn build_project_preset_plan(
    project_path: &Path,
    engines: &[EngineInstallation],
    preset_path: &Path,
    preset: &Preset,
    suppression: ProjectSuppressionEdit,
    options: &PlanBuildOptions,
) -> Result<ProjectPlan> {
    let prepared = prepare_project(project_path, engines)?;
    let plugin_names = prepared
        .plugins
        .iter()
        .map(|plugin| plugin.name.clone())
        .collect::<Vec<_>>();
    let resolution = resolve_project_preset(preset, &plugin_names, suppression)?;
    build_prepared_project_plan(
        prepared,
        ProjectPlanSource {
            name: preset.name.clone(),
            path: Some(preset_path.to_path_buf()),
        },
        resolution.edit,
        resolution.preset.pattern_expansions,
        resolution.preset.unmatched,
        options,
    )
}

/// Builds a project plan from explicit GUI or console edits without changing any file.
///
/// # Errors
///
/// Returns an error when project loading, engine resolution, scanning, or focused editing fails.
pub fn build_project_edit_plan(
    project_path: &Path,
    engines: &[EngineInstallation],
    source_name: &str,
    edit: ProjectDescriptorEdit,
    options: &PlanBuildOptions,
) -> Result<ProjectPlan> {
    if source_name.trim().is_empty() {
        return Err(Error::InvalidInput {
            message: "project plan source name is empty".to_owned(),
        });
    }
    build_prepared_project_plan(
        prepare_project(project_path, engines)?,
        ProjectPlanSource {
            name: source_name.to_owned(),
            path: None,
        },
        edit,
        Vec::new(),
        Vec::new(),
        options,
    )
}

struct PreparedProject {
    path: PathBuf,
    bytes: Vec<u8>,
    document: ProjectDescriptorDocument,
    engine: EngineInstallation,
    plugins: Vec<crate::descriptors::PluginDescriptor>,
    scan_warnings: Vec<PluginScanWarning>,
}

fn prepare_project(project_path: &Path, engines: &[EngineInstallation]) -> Result<PreparedProject> {
    let path = fs::canonicalize(project_path).map_err(|error| Error::NotFound {
        item: format!("project descriptor {}: {error}", project_path.display()),
    })?;
    let bytes = fs::read(&path).map_err(|error| Error::PermissionDenied {
        message: format!(
            "project descriptor read failed at {}: {error}",
            path.display()
        ),
    })?;
    let document =
        ProjectDescriptorDocument::parse(&bytes).map_err(|error| Error::InvalidInput {
            message: error.to_string(),
        })?;
    let engine = document
        .resolve_associated_engine(engines)
        .map_err(|error| Error::NotFound {
            item: error.to_string(),
        })?
        .clone();
    let scan = scan_engine_plugins(&engine)?;
    Ok(PreparedProject {
        path,
        bytes,
        document,
        engine,
        plugins: scan.plugins,
        scan_warnings: scan.warnings,
    })
}

fn build_prepared_project_plan(
    prepared: PreparedProject,
    source: ProjectPlanSource,
    edit: ProjectDescriptorEdit,
    pattern_expansions: Vec<PresetPatternExpansion>,
    unmatched_rules: Vec<UnmatchedPresetRule>,
    options: &PlanBuildOptions,
) -> Result<ProjectPlan> {
    let planned_bytes = prepared
        .document
        .edit(&edit)
        .map_err(|error| Error::InvalidInput {
            message: error.to_string(),
        })?;
    let planned_document =
        ProjectDescriptorDocument::parse(&planned_bytes).map_err(|error| Error::Internal {
            message: format!("planned project descriptor failed verification: {error}"),
        })?;
    let before = analyze_project_workspace(
        &prepared.path,
        &prepared.document,
        prepared.engine.clone(),
        prepared.plugins.clone(),
        prepared.scan_warnings.clone(),
    );
    let after = analyze_project_workspace(
        &prepared.path,
        &planned_document,
        prepared.engine.clone(),
        prepared.plugins,
        prepared.scan_warnings.clone(),
    );
    let plugins = planned_plugin_states(&before.plugins, &after.plugins);
    let impact = project_impact(&before, &after);
    let change = (planned_bytes != prepared.bytes).then(|| PlannedProjectFileEdit {
        path: prepared.path.clone(),
        relative_path: prepared
            .path
            .file_name()
            .map_or_else(PathBuf::new, PathBuf::from),
        sha256_before: sha256_hex(&prepared.bytes),
        sha256_after: sha256_hex(&planned_bytes),
        planned_byte_count: planned_bytes.len(),
        byte_change: differing_ranges(&prepared.bytes, &planned_bytes),
        planned_bytes,
    });
    if change
        .as_ref()
        .is_some_and(|file| file.relative_path.as_os_str().is_empty())
    {
        return Err(Error::InvalidInput {
            message: "selected project descriptor has no file name".to_owned(),
        });
    }
    Ok(ProjectPlan {
        schema: PLAN_SCHEMA,
        operation_id: options.operation_id().to_owned(),
        backup_directory: options.project_backup_directory(&prepared.path),
        project_path: prepared.path,
        engine: prepared.engine,
        source,
        edit,
        impact,
        plugins,
        change,
        scan_warnings: prepared.scan_warnings,
        dependency_warnings: after.dependency_warnings,
        project_warnings: after.project_warnings,
        pattern_expansions,
        unmatched_rules,
    })
}

fn planned_plugin_states(
    before: &[crate::project_state::ProjectPluginStatus],
    after: &[crate::project_state::ProjectPluginStatus],
) -> Vec<ProjectPlannedPlugin> {
    before
        .iter()
        .zip(after)
        .map(|(before, after)| ProjectPlannedPlugin {
            plugin: before.plugin.name.clone(),
            engine_effective_enabled: before.plugin.effective_enabled == Some(true),
            reference_before: before.project_reference,
            reference_after: after.project_reference,
            effective_before: before.project_effective_enabled,
            effective_after: after.project_effective_enabled,
            origin_before: before.project_origin,
            origin_after: after.project_origin,
        })
        .collect()
}

fn project_impact(
    before: &crate::project_state::ProjectWorkspace,
    after: &crate::project_state::ProjectWorkspace,
) -> ProjectPlanImpact {
    ProjectPlanImpact {
        effective_plugins: CountChange {
            before: effective_plugin_count(&before.plugins),
            after: effective_plugin_count(&after.plugins),
        },
        declared_modules: CountChange {
            before: effective_module_count(&before.plugins),
            after: effective_module_count(&after.plugins),
        },
        explicit_references: CountChange {
            before: before.project.plugins.len(),
            after: after.project.plugins.len(),
        },
    }
}

fn effective_plugin_count(plugins: &[crate::project_state::ProjectPluginStatus]) -> usize {
    plugins
        .iter()
        .filter(|plugin| plugin.project_effective_enabled)
        .count()
}

fn effective_module_count(plugins: &[crate::project_state::ProjectPluginStatus]) -> usize {
    plugins
        .iter()
        .filter(|plugin| plugin.project_effective_enabled)
        .map(|plugin| plugin.plugin.module_count)
        .sum()
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{build_project_edit_plan, build_project_preset_plan};
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use crate::plans::PlanBuildOptions;
    use crate::presets::PresetDocument;
    use crate::projects::{
        ProjectDescriptorEdit, ProjectPluginEdit, ProjectPluginEditAction, ProjectSuppressionEdit,
    };

    #[test]
    fn project_plan_keeps_exact_source_and_planned_hashes() -> Result<(), Box<dyn Error>> {
        let fixture = fixture()?;
        let options = PlanBuildOptions::new(fixture.backups.clone(), "project-plan".to_owned())?;
        let plan = build_project_edit_plan(
            &fixture.project,
            &[fixture.engine],
            "Manual project changes",
            ProjectDescriptorEdit {
                suppression: ProjectSuppressionEdit::Set(true),
                plugins: vec![ProjectPluginEdit {
                    plugin: "DefaultPlugin".to_owned(),
                    action: ProjectPluginEditAction::Disable,
                }],
            },
            &options,
        )?;
        let change = plan.change().ok_or("project change is missing")?;

        assert_eq!(
            change.sha256_before,
            crate::plans::sha256_hex(&fixture.source)
        );
        assert_eq!(
            change.sha256_after,
            crate::plans::sha256_hex(change.planned_bytes())
        );
        assert_eq!(plan.impact().effective_plugins.before, 1);
        assert_eq!(plan.impact().effective_plugins.after, 0);
        assert!(plan.backup_directory().starts_with(&fixture.backups));
        assert_eq!(fs::read(&fixture.project)?, fixture.source);
        Ok(())
    }

    #[test]
    fn project_preset_plan_reports_pattern_expansion_and_no_disk_write()
    -> Result<(), Box<dyn Error>> {
        let fixture = fixture()?;
        let preset = PresetDocument::parse(
            r#"
schema = 1
name = "Project preset"
enable = []
disable = []
clear = []
disable_matching = ["Default*"]
"#,
        )?;
        let options = PlanBuildOptions::new(fixture.backups.clone(), "preset-plan".to_owned())?;
        let preset_path = fixture.root.join("project-preset.toml");
        let plan = build_project_preset_plan(
            &fixture.project,
            &[fixture.engine],
            &preset_path,
            preset.preset(),
            ProjectSuppressionEdit::Keep,
            &options,
        )?;

        assert_eq!(plan.pattern_expansions()[0].matches, ["DefaultPlugin"]);
        assert!(plan.change().is_some());
        assert_eq!(fs::read(&fixture.project)?, fixture.source);
        Ok(())
    }

    struct Fixture {
        root: PathBuf,
        project: PathBuf,
        backups: PathBuf,
        engine: EngineInstallation,
        source: Vec<u8>,
    }

    fn fixture() -> Result<Fixture, Box<dyn Error>> {
        let temp = tempdir()?.keep();
        let plugin_directory = temp.join("Engine").join("Plugins").join("DefaultPlugin");
        fs::create_dir_all(&plugin_directory)?;
        fs::write(
            plugin_directory.join("DefaultPlugin.uplugin"),
            br#"{"EnabledByDefault":true,"Modules":[{"Name":"DefaultModule"}]}"#,
        )?;
        let source = br#"{"EngineAssociation":"5.8","SyntheticUnknown":{"Keep":true}}"#.to_vec();
        let project = temp.join("Synthetic.uproject");
        fs::write(&project, &source)?;
        let backups = temp.join("backups");
        Ok(Fixture {
            root: temp.clone(),
            project,
            backups,
            engine: EngineInstallation {
                path: temp,
                version: Some("5.8.1".to_owned()),
                source: DiscoverySource::Explicit,
                health: EngineHealth::Healthy,
                descriptor_count: 1,
                issues: Vec::new(),
            },
            source,
        })
    }
}
