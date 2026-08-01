//! Combines one project descriptor with its associated engine plugin state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Serialize;
use thiserror::Error;

use crate::dependencies::{
    BlockedDependency, DependencyWarning, EffectiveStatePolicy, analyze_plugins,
    analyze_plugins_with_policy,
};
use crate::descriptors::{
    DeclaredPluginState, PluginDescriptor, PluginScanWarning, scan_engine_plugins,
};
use crate::discovery::{EngineInstallation, EngineInstallation as DiscoveredEngine};
use crate::projects::{
    ProjectDescriptor, ProjectDescriptorDocument, ProjectDescriptorError, ProjectSuppressionState,
};

/// Identifies why a plugin has its current project state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPluginOrigin {
    /// Reports an explicit enabled reference in the `.uproject`.
    ProjectEnabled,
    /// Reports an explicit disabled reference in the `.uproject`.
    ProjectDisabled,
    /// Reports an unsuppressed `EnabledByDefault: true` engine descriptor.
    EngineDefault,
    /// Reports an engine default blocked by `DisableEnginePluginsByDefault`.
    EngineDefaultSuppressed,
    /// Reports a plugin enabled by another enabled plugin.
    Dependency,
    /// Reports a plugin with no active engine default, project reference, or dependency.
    NotEnabled,
}

impl ProjectPluginOrigin {
    /// Returns the stable lowercase identifier used in machine output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProjectEnabled => "project_enabled",
            Self::ProjectDisabled => "project_disabled",
            Self::EngineDefault => "engine_default",
            Self::EngineDefaultSuppressed => "engine_default_suppressed",
            Self::Dependency => "dependency",
            Self::NotEnabled => "not_enabled",
        }
    }
}

/// Describes one engine plugin after project overrides and dependencies determine its state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPluginStatus {
    /// Retains engine descriptor metadata and engine-level effective state.
    pub plugin: PluginDescriptor,
    /// Records an explicit project reference when present.
    pub project_reference: Option<bool>,
    /// Reports the effective state for the selected project.
    pub project_effective_enabled: bool,
    /// Identifies the rule that produced the project state.
    pub project_origin: ProjectPluginOrigin,
    /// Lists one stable root-to-plugin path for the project state.
    pub project_effective_path: Vec<String>,
    /// Lists effective plugins that directly depend on this plugin.
    pub project_reached_by: Vec<String>,
}

/// Identifies one warning produced while combining project and engine state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStateWarningCode {
    /// Reports a plugin that the project enables while disabling its dependency.
    DisabledRequiredDependency,
    /// Reports a project plugin reference absent from the selected engine scan.
    ReferenceNotInEngineScan,
}

impl ProjectStateWarningCode {
    /// Returns the stable lowercase identifier used in machine output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DisabledRequiredDependency => "disabled_required_dependency",
            Self::ReferenceNotInEngineScan => "reference_not_in_engine_scan",
        }
    }
}

/// Reports a project override or reference that needs review.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectStateWarning {
    /// Identifies the warning category.
    pub code: ProjectStateWarningCode,
    /// Names the project plugin reference or enabled dependent plugin.
    pub plugin: String,
    /// Names the blocked dependency when the warning concerns one edge.
    pub dependency: Option<String>,
    /// Reports whether Unreal marks the dependency reference as optional.
    pub optional: bool,
    /// Reports whether the condition can prevent a valid project load or build.
    pub blocking: bool,
    /// States the condition and recovery action.
    pub message: String,
}

/// Groups one selected project, its engine, and the combined plugin state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectWorkspace {
    /// Retains the selected project fields.
    pub project: ProjectDescriptor,
    /// Retains the resolved engine installation.
    pub engine: EngineInstallation,
    /// Lists engine plugins with both engine and project state.
    pub plugins: Vec<ProjectPluginStatus>,
    /// Lists plugin scan warnings from the associated engine.
    pub scan_warnings: Vec<PluginScanWarning>,
    /// Lists duplicate, missing, and ambiguous engine dependency warnings.
    pub dependency_warnings: Vec<DependencyWarning>,
    /// Lists project override conflicts and references outside the engine scan.
    pub project_warnings: Vec<ProjectStateWarning>,
}

/// Reports project workspace load failures.
#[derive(Debug, Error)]
pub enum ProjectWorkspaceError {
    /// Retains a project selection, parse, or association failure.
    #[error(transparent)]
    Project(#[from] ProjectDescriptorError),
    /// Retains an associated engine scan failure.
    #[error(transparent)]
    Engine(#[from] crate::Error),
}

/// Loads one project, resolves its engine, and combines both plugin layers without writing files.
///
/// # Errors
///
/// Returns an error when project loading, engine resolution, or the engine plugin scan fails.
pub fn load_project_workspace(
    project_path: &Path,
    engines: &[DiscoveredEngine],
) -> Result<ProjectWorkspace, ProjectWorkspaceError> {
    let document = ProjectDescriptorDocument::load(project_path)?;
    let engine = document.resolve_associated_engine(engines)?.clone();
    let scan = scan_engine_plugins(&engine)?;
    Ok(analyze_project_workspace(
        project_path,
        &document,
        engine,
        scan.plugins,
        scan.warnings,
    ))
}

/// Combines parsed project fields with a supplied engine plugin scan.
#[must_use]
pub fn analyze_project_workspace(
    project_path: &Path,
    document: &ProjectDescriptorDocument,
    engine: EngineInstallation,
    plugins: Vec<PluginDescriptor>,
    scan_warnings: Vec<PluginScanWarning>,
) -> ProjectWorkspace {
    let project = document.project_descriptor(project_path);
    let references = project
        .plugins
        .iter()
        .map(|reference| (normalize_name(&reference.name), reference.enabled))
        .collect::<BTreeMap<_, _>>();
    let blocked = references
        .iter()
        .filter(|(_, enabled)| !**enabled)
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    let suppresses_engine_defaults = project.suppression == ProjectSuppressionState::Enabled;
    let roots = plugins
        .iter()
        .filter(
            |plugin| match references.get(&normalize_name(&plugin.name)).copied() {
                Some(enabled) => enabled,
                None => {
                    !suppresses_engine_defaults
                        && plugin.declared_state == DeclaredPluginState::Enabled
                }
            },
        )
        .map(|plugin| plugin.name.clone())
        .collect::<Vec<_>>();
    let policy = EffectiveStatePolicy::new(roots, blocked);
    let engine_report = analyze_plugins(plugins.clone());
    let (project_report, blocked_dependencies) = analyze_plugins_with_policy(plugins, &policy);

    let statuses = engine_report
        .plugins
        .into_iter()
        .zip(project_report.plugins)
        .map(|(engine_plugin, project_plugin)| {
            let project_reference = references
                .get(&normalize_name(&engine_plugin.name))
                .copied();
            let project_effective_enabled = project_plugin.effective_enabled == Some(true);
            let project_origin = project_origin(
                &engine_plugin,
                &project_plugin,
                project_reference,
                suppresses_engine_defaults,
            );
            ProjectPluginStatus {
                plugin: engine_plugin,
                project_reference,
                project_effective_enabled,
                project_origin,
                project_effective_path: project_plugin.effective_path,
                project_reached_by: project_plugin.reached_by,
            }
        })
        .collect::<Vec<_>>();
    let project_warnings = build_project_warnings(&project, &statuses, blocked_dependencies);

    ProjectWorkspace {
        project,
        engine,
        plugins: statuses,
        scan_warnings,
        dependency_warnings: project_report.warnings,
        project_warnings,
    }
}

fn build_project_warnings(
    project: &ProjectDescriptor,
    statuses: &[ProjectPluginStatus],
    blocked_dependencies: Vec<BlockedDependency>,
) -> Vec<ProjectStateWarning> {
    let engine_names = statuses
        .iter()
        .map(|status| normalize_name(&status.plugin.name))
        .collect::<BTreeSet<_>>();
    let mut warnings = blocked_dependencies
        .into_iter()
        .map(|blocked| ProjectStateWarning {
            code: ProjectStateWarningCode::DisabledRequiredDependency,
            plugin: blocked.plugin.clone(),
            dependency: Some(blocked.dependency.clone()),
            optional: blocked.optional,
            blocking: !blocked.optional,
            message: blocked_dependency_message(
                &blocked.plugin,
                &blocked.dependency,
                blocked.optional,
            ),
        })
        .collect::<Vec<_>>();
    warnings.extend(
        project
            .plugins
            .iter()
            .filter(|reference| !engine_names.contains(&normalize_name(&reference.name)))
            .map(|reference| ProjectStateWarning {
                code: ProjectStateWarningCode::ReferenceNotInEngineScan,
                plugin: reference.name.clone(),
                dependency: None,
                optional: false,
                blocking: false,
                message: format!(
                    "Engine scan does not contain project reference {} under the selected engine's Engine\\Plugins directory. Check the project's Plugins and AdditionalPluginDirectories before treating the reference as stale.",
                    reference.name
                ),
            }),
    );
    warnings.sort_by(|left, right| {
        normalize_name(&left.plugin)
            .cmp(&normalize_name(&right.plugin))
            .then_with(|| left.code.as_str().cmp(right.code.as_str()))
            .then_with(|| left.dependency.cmp(&right.dependency))
    });
    warnings
}

fn project_origin(
    engine_plugin: &PluginDescriptor,
    project_plugin: &PluginDescriptor,
    project_reference: Option<bool>,
    suppresses_engine_defaults: bool,
) -> ProjectPluginOrigin {
    match project_reference {
        Some(true) => ProjectPluginOrigin::ProjectEnabled,
        Some(false) => ProjectPluginOrigin::ProjectDisabled,
        None if project_plugin.effective_enabled == Some(true) => {
            if project_plugin.effective_path.len() == 1
                && engine_plugin.declared_state == DeclaredPluginState::Enabled
                && !suppresses_engine_defaults
            {
                ProjectPluginOrigin::EngineDefault
            } else {
                ProjectPluginOrigin::Dependency
            }
        }
        None if suppresses_engine_defaults
            && engine_plugin.declared_state == DeclaredPluginState::Enabled =>
        {
            ProjectPluginOrigin::EngineDefaultSuppressed
        }
        None => ProjectPluginOrigin::NotEnabled,
    }
}

fn blocked_dependency_message(plugin: &str, dependency: &str, optional: bool) -> String {
    if optional {
        format!(
            "Project disables an optional dependency: {plugin} references {dependency}. Review the feature before keeping the explicit disabled reference."
        )
    } else {
        format!(
            "Project disables a required dependency: {plugin} requires {dependency}. Enable {dependency} or disable {plugin} before opening or building the project."
        )
    }
}

fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{ProjectPluginOrigin, ProjectStateWarningCode, analyze_project_workspace};
    use crate::descriptors::{DescriptorDocument, PluginDescriptor};
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use crate::projects::ProjectDescriptorDocument;

    #[test]
    fn project_state_exposes_engine_defaults_suppression_and_explicit_overrides()
    -> Result<(), Box<dyn Error>> {
        let project = ProjectDescriptorDocument::parse(
            br#"{
                "EngineAssociation": "5.8",
                "DisableEnginePluginsByDefault": true,
                "Plugins": [
                    {"Name":"EngineDefault","Enabled":false},
                    {"Name":"EngineOff","Enabled":true}
                ]
            }"#,
        )?;
        let workspace = analyze_project_workspace(
            Path::new("Synthetic.uproject"),
            &project,
            engine(),
            vec![
                plugin("EngineDefault", true, &[])?,
                plugin("EngineOff", false, &[])?,
                plugin("SuppressedDefault", true, &[])?,
                plugin("Unspecified", false, &[])?,
            ],
            Vec::new(),
        );

        let engine_default = status(&workspace, "EngineDefault")?;
        assert_eq!(engine_default.plugin.effective_enabled, Some(true));
        assert!(!engine_default.project_effective_enabled);
        assert_eq!(
            engine_default.project_origin,
            ProjectPluginOrigin::ProjectDisabled
        );
        let engine_off = status(&workspace, "EngineOff")?;
        assert_eq!(engine_off.plugin.effective_enabled, Some(false));
        assert!(engine_off.project_effective_enabled);
        assert_eq!(
            engine_off.project_origin,
            ProjectPluginOrigin::ProjectEnabled
        );
        assert_eq!(
            status(&workspace, "SuppressedDefault")?.project_origin,
            ProjectPluginOrigin::EngineDefaultSuppressed
        );
        assert_eq!(
            status(&workspace, "Unspecified")?.project_origin,
            ProjectPluginOrigin::NotEnabled
        );
        Ok(())
    }

    #[test]
    fn project_dependencies_enable_unblocked_plugins_and_report_disabled_requirements()
    -> Result<(), Box<dyn Error>> {
        let project = ProjectDescriptorDocument::parse(
            br#"{
                "EngineAssociation": "5.8",
                "DisableEnginePluginsByDefault": true,
                "Plugins": [
                    {"Name":"Root","Enabled":true},
                    {"Name":"Blocked","Enabled":false}
                ]
            }"#,
        )?;
        let workspace = analyze_project_workspace(
            Path::new("Synthetic.uproject"),
            &project,
            engine(),
            vec![
                plugin("Root", false, &[("Transitive", false), ("Blocked", false)])?,
                plugin("Transitive", false, &[])?,
                plugin("Blocked", false, &[])?,
            ],
            Vec::new(),
        );

        let transitive = status(&workspace, "Transitive")?;
        assert!(transitive.project_effective_enabled);
        assert_eq!(transitive.project_origin, ProjectPluginOrigin::Dependency);
        assert_eq!(transitive.project_effective_path, ["Root", "Transitive"]);
        let blocked = status(&workspace, "Blocked")?;
        assert!(!blocked.project_effective_enabled);
        assert_eq!(blocked.project_origin, ProjectPluginOrigin::ProjectDisabled);
        assert!(workspace.project_warnings.iter().any(|warning| {
            warning.code == ProjectStateWarningCode::DisabledRequiredDependency
                && warning.plugin == "Root"
                && warning.dependency.as_deref() == Some("Blocked")
                && warning.blocking
        }));
        Ok(())
    }

    #[test]
    fn project_references_outside_engine_plugins_remain_visible() -> Result<(), Box<dyn Error>> {
        let project = ProjectDescriptorDocument::parse(
            br#"{
                "EngineAssociation": "5.8",
                "Plugins": [{"Name":"ProjectOnly","Enabled":true}]
            }"#,
        )?;
        let workspace = analyze_project_workspace(
            Path::new("Synthetic.uproject"),
            &project,
            engine(),
            vec![plugin("EnginePlugin", true, &[])?],
            Vec::new(),
        );

        assert!(workspace.project_warnings.iter().any(|warning| {
            warning.code == ProjectStateWarningCode::ReferenceNotInEngineScan
                && warning.plugin == "ProjectOnly"
                && !warning.blocking
        }));
        Ok(())
    }

    #[test]
    fn loading_a_project_resolves_and_scans_its_associated_engine() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let plugin_directory = temp
            .path()
            .join("Engine")
            .join("Plugins")
            .join("SyntheticPlugin");
        fs::create_dir_all(&plugin_directory)?;
        fs::write(
            plugin_directory.join("SyntheticPlugin.uplugin"),
            br#"{"EnabledByDefault":true}"#,
        )?;
        let project_path = temp.path().join("Synthetic.uproject");
        fs::write(
            &project_path,
            br#"{"EngineAssociation":"5.8","Plugins":[{"Name":"SyntheticPlugin","Enabled":false}]}"#,
        )?;
        let engine = EngineInstallation {
            path: temp.path().to_path_buf(),
            version: Some("5.8.1".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 1,
            issues: Vec::new(),
        };

        let workspace = super::load_project_workspace(&project_path, &[engine])?;

        assert_eq!(workspace.engine.version.as_deref(), Some("5.8.1"));
        assert_eq!(workspace.plugins.len(), 1);
        assert_eq!(
            workspace.plugins[0].project_origin,
            ProjectPluginOrigin::ProjectDisabled
        );
        Ok(())
    }

    fn plugin(
        name: &str,
        enabled: bool,
        dependencies: &[(&str, bool)],
    ) -> Result<PluginDescriptor, Box<dyn Error>> {
        let references = dependencies
            .iter()
            .map(|(dependency, optional)| {
                format!("{{\"Name\":\"{dependency}\",\"Enabled\":true,\"Optional\":{optional}}}")
            })
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("{{\"EnabledByDefault\":{enabled},\"Plugins\":[{references}]}}");
        let file_name = format!("{name}.uplugin");
        Ok(DescriptorDocument::parse(source.as_bytes())?
            .plugin_descriptor(Path::new(&file_name), Path::new("")))
    }

    fn engine() -> EngineInstallation {
        EngineInstallation {
            path: PathBuf::from("D:\\Synthetic\\UE_5.8"),
            version: Some("5.8.1".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 4,
            issues: Vec::new(),
        }
    }

    fn status<'a>(
        workspace: &'a super::ProjectWorkspace,
        name: &str,
    ) -> Result<&'a super::ProjectPluginStatus, Box<dyn Error>> {
        workspace
            .plugins
            .iter()
            .find(|status| status.plugin.name == name)
            .ok_or_else(|| format!("plugin {name} is missing").into())
    }
}
