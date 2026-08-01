//! Connects desktop actions to the shared scan, plan, transaction, and recovery APIs.

use std::collections::BTreeMap;
use std::path::Path;

use unclean_core::apply::{
    OperationReport, ProjectRestorePlan, RestorePlan, TemplateRestorePlan, apply_engine_plan,
    apply_project_plan, apply_template_plan, build_project_restore_plan, build_restore_plan,
    build_template_restore_plan, restore_engine_plan, restore_project_plan, restore_template_plan,
};
use unclean_core::dependencies::{DependencyWarning, analyze_plugins};
use unclean_core::descriptors::{PluginDescriptor, PluginScanWarning, scan_engine_plugins};
use unclean_core::discovery::EngineInstallation;
use unclean_core::elevation::{
    ActiveUnrealProcess, ElevatedRequest, find_active_unreal_processes, run_elevated_request,
    template_write_access_requires_elevation, write_access_requires_elevation,
};
use unclean_core::journal::{
    EngineStatus, JournalOperation, ProjectStatus, default_journal_path, engine_history,
    inspect_engine_status, inspect_project_status, inspect_template_status, project_history,
    template_history,
};
use unclean_core::plans::{EnginePlan, PlanBuildOptions, build_engine_plan};
use unclean_core::presets::PresetDocument;
use unclean_core::project_plans::{ProjectPlan, build_project_preset_plan};
use unclean_core::project_state::{ProjectWorkspace, load_project_workspace};
use unclean_core::projects::ProjectSuppressionEdit;
use unclean_core::templates::{
    TemplateCatalog, TemplatePlan, build_template_plan, scan_engine_templates,
};
use unclean_core::{Error, Result};

/// Supplies one engine view with plugin state, drift, and history.
#[derive(Clone, Debug)]
pub struct EngineWorkspace {
    /// Lists parsed plugins with effective dependency state.
    pub plugins: Vec<PluginDescriptor>,
    /// Lists descriptor paths that the scan skipped.
    pub scan_warnings: Vec<PluginScanWarning>,
    /// Lists unresolved dependency references.
    pub dependency_warnings: Vec<DependencyWarning>,
    /// Reports drift against the latest recorded operation.
    pub status: EngineStatus,
    /// Lists completed operations newest first.
    pub history: Vec<JournalOperation>,
}

/// Supplies one project view with combined plugin state, drift, and history.
#[derive(Clone, Debug)]
pub struct ProjectWorkspaceView {
    /// Retains the selected project, associated engine, and combined plugin states.
    pub workspace: ProjectWorkspace,
    /// Reports drift against the latest recorded project operation.
    pub status: ProjectStatus,
    /// Lists completed project operations newest first.
    pub history: Vec<JournalOperation>,
}

/// Supplies one engine template view with drift and operation history.
#[derive(Clone, Debug)]
pub struct TemplateWorkspaceView {
    /// Lists valid project templates and scan findings.
    pub catalog: TemplateCatalog,
    /// Reports drift against the latest template operation.
    pub status: EngineStatus,
    /// Lists completed template operations newest first.
    pub history: Vec<JournalOperation>,
}

/// Supplies one engine plan and its write-access result for multi-engine review.
#[derive(Clone, Debug)]
pub struct ReviewedEnginePlan {
    /// Retains the immutable plan shown before writing.
    pub plan: EnginePlan,
    /// Reports whether this engine needs administrator approval.
    pub requires_elevation: Option<bool>,
}

/// Scans one engine and loads its dependency, drift, and history data.
///
/// # Errors
///
/// Returns an error when plugin scanning or journal inspection fails.
pub fn load_engine_workspace(engine: &EngineInstallation) -> Result<EngineWorkspace> {
    let scan = scan_engine_plugins(engine)?;
    let dependency_report = analyze_plugins(scan.plugins);
    let journal_path = default_journal_path()?;
    Ok(EngineWorkspace {
        plugins: dependency_report.plugins,
        scan_warnings: scan.warnings,
        dependency_warnings: dependency_report.warnings,
        status: inspect_engine_status(engine, &journal_path)?,
        history: engine_history(engine, &journal_path)?,
    })
}

/// Loads one project, resolves its engine, and combines engine defaults with project overrides.
///
/// # Errors
///
/// Returns an error when project loading, engine resolution, plugin scanning, or journal loading fails.
pub fn load_project_workspace_view(
    project_path: &Path,
    engines: &[EngineInstallation],
) -> Result<ProjectWorkspaceView> {
    let workspace =
        load_project_workspace(project_path, engines).map_err(|error| Error::InvalidInput {
            message: error.to_string(),
        })?;
    let journal_path = default_journal_path()?;
    Ok(ProjectWorkspaceView {
        status: inspect_project_status(project_path, &journal_path)?,
        history: project_history(project_path, &journal_path)?,
        workspace,
    })
}

/// Loads project templates, drift, and history for one engine.
///
/// # Errors
///
/// Returns an error when template scanning or journal inspection fails.
pub fn load_template_workspace(engine: &EngineInstallation) -> Result<TemplateWorkspaceView> {
    let journal_path = default_journal_path()?;
    Ok(TemplateWorkspaceView {
        catalog: scan_engine_templates(engine)?,
        status: inspect_template_status(engine, &journal_path)?,
        history: template_history(engine, &journal_path)?,
    })
}

/// Builds the immutable engine plan rendered by the desktop review sheet.
///
/// # Errors
///
/// Returns an error when the preset cannot resolve or current descriptor state changed.
pub fn build_engine_review(
    engine: &EngineInstallation,
    preset_path: &Path,
    document: &PresetDocument,
) -> Result<EnginePlan> {
    let options = PlanBuildOptions::for_current_process()?;
    build_engine_review_with_options(engine, preset_path, document, &options)
}

/// Builds a desktop engine plan with caller-supplied paths for deterministic contract tests.
///
/// # Errors
///
/// Returns an error when the preset cannot resolve or current descriptor state changed.
pub fn build_engine_review_with_options(
    engine: &EngineInstallation,
    preset_path: &Path,
    document: &PresetDocument,
    options: &PlanBuildOptions,
) -> Result<EnginePlan> {
    build_engine_plan(engine, preset_path, document.preset(), options)
}

/// Builds separate reviewed plans for an explicit engine selection.
///
/// # Errors
///
/// Returns an error when any selected engine cannot produce a plan.
pub fn build_multi_engine_review(
    engines: &[EngineInstallation],
    preset_path: &Path,
    document: &PresetDocument,
) -> Result<Vec<ReviewedEnginePlan>> {
    let options = (0..engines.len())
        .map(|_| PlanBuildOptions::for_current_process())
        .collect::<Result<Vec<_>>>()?;
    build_multi_engine_review_with_options(engines, preset_path, document, &options)
}

/// Builds separate engine plans with caller-supplied operation paths.
///
/// # Errors
///
/// Returns an error when option counts differ or any selected engine cannot produce a plan.
pub fn build_multi_engine_review_with_options(
    engines: &[EngineInstallation],
    preset_path: &Path,
    document: &PresetDocument,
    options: &[PlanBuildOptions],
) -> Result<Vec<ReviewedEnginePlan>> {
    if engines.len() != options.len() {
        return Err(Error::InvalidInput {
            message:
                "Multi-engine review options do not match the engine selection. Rebuild the review."
                    .to_owned(),
        });
    }
    let mut plans = Vec::with_capacity(engines.len());
    for (engine, options) in engines.iter().zip(options) {
        let plan = build_engine_review_with_options(engine, preset_path, document, options)
            .map_err(|error| Error::InvalidInput {
                message: format!(
                    "Multi-engine review failed for UE {}: {error}. Check this installation and retry.",
                    engine.version.as_deref().unwrap_or("unknown")
                ),
            })?;
        plans.push(ReviewedEnginePlan {
            requires_elevation: engine_review_requires_elevation(&plan).ok(),
            plan,
        });
    }
    Ok(plans)
}

/// Builds the immutable project plan rendered by the desktop review sheet.
///
/// # Errors
///
/// Returns an error when project loading, engine resolution, scanning, or preset mapping fails.
pub fn build_project_review(
    project_path: &Path,
    engines: &[EngineInstallation],
    preset_path: &Path,
    document: &PresetDocument,
    suppression: ProjectSuppressionEdit,
) -> Result<ProjectPlan> {
    let options = PlanBuildOptions::for_current_process()?;
    build_project_review_with_options(
        project_path,
        engines,
        preset_path,
        document,
        suppression,
        &options,
    )
}

/// Builds a desktop project plan with caller-supplied paths for deterministic contract tests.
///
/// # Errors
///
/// Returns an error when project loading, engine resolution, scanning, or preset mapping fails.
pub fn build_project_review_with_options(
    project_path: &Path,
    engines: &[EngineInstallation],
    preset_path: &Path,
    document: &PresetDocument,
    suppression: ProjectSuppressionEdit,
    options: &PlanBuildOptions,
) -> Result<ProjectPlan> {
    build_project_preset_plan(
        project_path,
        engines,
        preset_path,
        document.preset(),
        suppression,
        options,
    )
}

/// Builds the immutable template plan rendered by the desktop review sheet.
///
/// # Errors
///
/// Returns an error when template discovery, selection, or focused editing fails.
pub fn build_template_review(
    engine: &EngineInstallation,
    selected_relative_paths: &[std::path::PathBuf],
    suppression: ProjectSuppressionEdit,
) -> Result<TemplatePlan> {
    let options = PlanBuildOptions::for_current_process()?;
    build_template_review_with_options(engine, selected_relative_paths, suppression, &options)
}

/// Builds a desktop template plan with caller-supplied operation paths.
///
/// # Errors
///
/// Returns an error when template discovery, selection, or focused editing fails.
pub fn build_template_review_with_options(
    engine: &EngineInstallation,
    selected_relative_paths: &[std::path::PathBuf],
    suppression: ProjectSuppressionEdit,
    options: &PlanBuildOptions,
) -> Result<TemplatePlan> {
    build_template_plan(engine, selected_relative_paths, suppression, options)
}

/// Builds the immutable restore plan rendered by the desktop review sheet.
///
/// # Errors
///
/// Returns an error when the snapshot, manifest, or recovery bytes fail validation.
pub fn build_restore_review(engine: &EngineInstallation, snapshot: &str) -> Result<RestorePlan> {
    let options = PlanBuildOptions::for_current_process()?;
    build_restore_plan(engine, snapshot, &default_journal_path()?, &options)
}

/// Builds the immutable project restore plan rendered by the desktop review sheet.
///
/// # Errors
///
/// Returns an error when the project snapshot, manifest, or recovery bytes fail validation.
pub fn build_project_restore_review(
    project_path: &Path,
    engines: &[EngineInstallation],
    snapshot: &str,
) -> Result<ProjectRestorePlan> {
    let options = PlanBuildOptions::for_current_process()?;
    build_project_restore_plan(
        project_path,
        engines,
        snapshot,
        &default_journal_path()?,
        &options,
    )
}

/// Builds the immutable template restore plan rendered by the desktop review sheet.
///
/// # Errors
///
/// Returns an error when snapshot metadata or template recovery bytes fail validation.
pub fn build_template_restore_review(
    engine: &EngineInstallation,
    snapshot: &str,
) -> Result<TemplateRestorePlan> {
    let options = PlanBuildOptions::for_current_process()?;
    build_template_restore_plan(engine, snapshot, &default_journal_path()?, &options)
}

/// Reports whether an engine plan needs an elevated writer.
///
/// # Errors
///
/// Returns an error when the write-access probe cannot inspect a target.
pub fn engine_review_requires_elevation(plan: &EnginePlan) -> Result<bool> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    write_access_requires_elevation(plan.engine(), &relative_paths)
}

/// Reports whether a restore plan needs an elevated writer.
///
/// # Errors
///
/// Returns an error when the write-access probe cannot inspect a target.
pub fn restore_review_requires_elevation(plan: &RestorePlan) -> Result<bool> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    write_access_requires_elevation(plan.engine(), &relative_paths)
}

/// Reports whether a template plan needs an elevated writer.
///
/// # Errors
///
/// Returns an error when the write-access probe cannot inspect a target.
pub fn template_review_requires_elevation(plan: &TemplatePlan) -> Result<bool> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    template_write_access_requires_elevation(plan.engine(), &relative_paths)
}

/// Reports whether a template restore plan needs an elevated writer.
///
/// # Errors
///
/// Returns an error when the write-access probe cannot inspect a target.
pub fn template_restore_requires_elevation(plan: &TemplateRestorePlan) -> Result<bool> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    template_write_access_requires_elevation(plan.engine(), &relative_paths)
}

/// Lists active Unreal processes that require a second desktop confirmation.
///
/// # Errors
///
/// Returns an error when Windows process enumeration cannot start.
pub fn active_engine_processes(engine: &EngineInstallation) -> Result<Vec<ActiveUnrealProcess>> {
    find_active_unreal_processes(&engine.path)
}

/// Lists each active Unreal process once across a selected engine set.
///
/// # Errors
///
/// Returns an error when process enumeration fails for any engine.
pub fn active_multi_engine_processes(
    plans: &[ReviewedEnginePlan],
) -> Result<Vec<ActiveUnrealProcess>> {
    let mut processes = BTreeMap::new();
    for reviewed in plans {
        for process in active_engine_processes(reviewed.plan.engine())? {
            processes.entry(process.process_id).or_insert(process);
        }
    }
    Ok(processes.into_values().collect())
}

/// Applies one confirmed desktop plan through the direct or elevated writer.
///
/// Call this only after the review sheet and active-process confirmation complete.
///
/// # Errors
///
/// Returns an error when access checks, elevation, backup, replacement, verification, or journaling fails.
pub fn execute_engine_review(plan: &EnginePlan) -> Result<OperationReport> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    if write_access_requires_elevation(plan.engine(), &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_engine_plan(plan)?)
    } else {
        apply_engine_plan(plan, &default_journal_path()?)
    }
}

/// Restores one confirmed desktop plan through the direct or elevated writer.
///
/// Call this only after the review sheet and active-process confirmation complete.
///
/// # Errors
///
/// Returns an error when access checks, elevation, backup, replacement, verification, or journaling fails.
pub fn execute_restore_review(plan: &RestorePlan) -> Result<OperationReport> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    if write_access_requires_elevation(plan.engine(), &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_restore_plan(plan)?)
    } else {
        restore_engine_plan(plan, &default_journal_path()?)
    }
}

/// Applies one confirmed project plan through the protected project writer.
///
/// Call this only after the review sheet and active-process confirmation complete.
///
/// # Errors
///
/// Returns an error when backup, replacement, verification, or journaling fails.
pub fn execute_project_review(plan: &ProjectPlan) -> Result<OperationReport> {
    apply_project_plan(plan, &default_journal_path()?)
}

/// Restores one confirmed project snapshot through the protected project writer.
///
/// Call this only after the review sheet and active-process confirmation complete.
///
/// # Errors
///
/// Returns an error when backup, replacement, verification, or journaling fails.
pub fn execute_project_restore_review(plan: &ProjectRestorePlan) -> Result<OperationReport> {
    restore_project_plan(plan, &default_journal_path()?)
}

/// Applies one confirmed template plan through the direct or elevated writer.
///
/// # Errors
///
/// Returns an error when elevation, backup, replacement, verification, or journaling fails.
pub fn execute_template_review(plan: &TemplatePlan) -> Result<OperationReport> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    if template_write_access_requires_elevation(plan.engine(), &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_template_plan(plan)?)
    } else {
        apply_template_plan(plan, &default_journal_path()?)
    }
}

/// Restores one confirmed template snapshot through the direct or elevated writer.
///
/// # Errors
///
/// Returns an error when elevation, backup, replacement, verification, or journaling fails.
pub fn execute_template_restore_review(plan: &TemplateRestorePlan) -> Result<OperationReport> {
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    if template_write_access_requires_elevation(plan.engine(), &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_template_restore_plan(plan)?)
    } else {
        restore_template_plan(plan, &default_journal_path()?)
    }
}

/// Computes effective plugin state after applying the reviewed descriptor changes in memory.
#[must_use]
pub fn projected_effective_states(
    plugins: &[PluginDescriptor],
    plan: Option<&EnginePlan>,
) -> BTreeMap<String, bool> {
    let mut projected = plugins.to_vec();
    if let Some(plan) = plan {
        for change in plan.changes() {
            if let Some(plugin) = projected
                .iter_mut()
                .find(|plugin| plugin.name.eq_ignore_ascii_case(&change.plugin))
            {
                plugin.declared_state = change.value_after;
            }
        }
    }
    analyze_plugins(projected)
        .plugins
        .into_iter()
        .map(|plugin| (plugin.name, plugin.effective_enabled.unwrap_or(false)))
        .collect()
}

/// Matches the plugin fields searched by the desktop list.
#[must_use]
pub fn plugin_matches(plugin: &PluginDescriptor, query: &str) -> bool {
    let query = query.trim().to_ascii_lowercase();
    query.is_empty()
        || [
            plugin.name.as_str(),
            plugin.friendly_name.as_str(),
            plugin.category.as_deref().unwrap_or_default(),
            plugin.description.as_deref().unwrap_or_default(),
        ]
        .iter()
        .any(|value| value.to_ascii_lowercase().contains(&query))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;
    use unclean_core::descriptors::scan_engine_plugins;
    use unclean_core::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use unclean_core::plans::PlanBuildOptions;
    use unclean_core::presets::PresetDocument;
    use unclean_core::projects::ProjectSuppressionEdit;

    use super::{
        build_engine_review_with_options, build_multi_engine_review_with_options,
        build_project_review_with_options, build_template_review_with_options, plugin_matches,
        projected_effective_states,
    };

    #[test]
    fn search_and_projection_use_shared_descriptor_state() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_Invented");
        let plugin_dir = engine_path.join("Engine").join("Plugins");
        let root = plugin_dir.join("Root").join("Root.uplugin");
        let dependency = plugin_dir.join("Dependency").join("Dependency.uplugin");
        fs::create_dir_all(root.parent().ok_or("root has no parent")?)?;
        fs::create_dir_all(dependency.parent().ok_or("dependency has no parent")?)?;
        fs::write(
            &root,
            r#"{"FriendlyName":"Root Tool","Category":"Testing","EnabledByDefault":true,"Plugins":[{"Name":"Dependency","Enabled":true}]}"#,
        )?;
        fs::write(
            &dependency,
            r#"{"FriendlyName":"Dependency Tool","EnabledByDefault":false}"#,
        )?;
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.9.0-test".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 2,
            issues: Vec::new(),
        };
        let plugins = scan_engine_plugins(&engine)?.plugins;
        assert!(plugin_matches(&plugins[0], "tool"));
        assert!(
            plugins
                .iter()
                .any(|plugin| plugin_matches(plugin, "testing"))
        );

        let document = PresetDocument::parse(
            "schema = 1\nname = \"Projection\"\nenable = []\ndisable = [\"Root\"]\nclear = []\ndisable_matching = []\n",
        )?;
        let options =
            PlanBuildOptions::new(temp.path().join("backups"), "projection-test".to_owned())?;
        let plan = build_engine_review_with_options(
            &engine,
            &temp.path().join("projection.toml"),
            &document,
            &options,
        )?;
        let projected = projected_effective_states(&plugins, Some(&plan));
        assert_eq!(projected.get("Root"), Some(&false));
        assert_eq!(projected.get("Dependency"), Some(&false));
        Ok(())
    }

    #[test]
    fn project_review_uses_the_shared_engine_and_project_result()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_5.8");
        let plugin = engine_path
            .join("Engine")
            .join("Plugins")
            .join("DefaultPlugin")
            .join("DefaultPlugin.uplugin");
        fs::create_dir_all(plugin.parent().ok_or("plugin has no parent")?)?;
        fs::write(&plugin, r#"{"EnabledByDefault":true}"#)?;
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.8.0-test".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 1,
            issues: Vec::new(),
        };
        let project_path = temp.path().join("Fixture.uproject");
        fs::write(
            &project_path,
            r#"{"EngineAssociation":"5.8","Plugins":[{"Name":"DefaultPlugin","Enabled":false}]}"#,
        )?;
        let document = PresetDocument::parse(
            "schema = 1\nname = \"Project review\"\nenable = []\ndisable = []\nclear = [\"DefaultPlugin\"]\ndisable_matching = []\n",
        )?;
        let options =
            PlanBuildOptions::new(temp.path().join("backups"), "project-review".to_owned())?;

        let plan = build_project_review_with_options(
            &project_path,
            &[engine],
            &temp.path().join("project.toml"),
            &document,
            ProjectSuppressionEdit::Keep,
            &options,
        )?;
        let plugin = &plan.plugins()[0];

        assert!(plugin.engine_effective_enabled);
        assert_eq!(plugin.reference_before, Some(false));
        assert_eq!(plugin.reference_after, None);
        assert!(!plugin.effective_before);
        assert!(plugin.effective_after);
        assert!(plan.change().is_some());
        Ok(())
    }

    #[test]
    fn template_review_requires_an_explicit_descriptor_selection()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let engine_path = temp.path().join("UE_5.8");
        let template = engine_path
            .join("Templates")
            .join("TP_Blank")
            .join("TP_Blank.uproject");
        fs::create_dir_all(template.parent().ok_or("template has no parent")?)?;
        fs::write(
            &template,
            r#"{"FileVersion":3,"Plugins":[{"Name":"KeepMe","Enabled":true}]}"#,
        )?;
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.8.0-test".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 0,
            issues: Vec::new(),
        };
        let options =
            PlanBuildOptions::new(temp.path().join("backups"), "template-review".to_owned())?;

        let plan = build_template_review_with_options(
            &engine,
            &[PathBuf::from("Templates/TP_Blank/TP_Blank.uproject")],
            ProjectSuppressionEdit::Set(true),
            &options,
        )?;

        assert_eq!(plan.templates().len(), 1);
        assert_eq!(plan.changes().len(), 1);
        let planned = std::str::from_utf8(plan.changes()[0].planned_bytes())?;
        assert!(planned.contains(r#""DisableEnginePluginsByDefault":true"#));
        assert!(planned.contains(r#""Name":"KeepMe","Enabled":true"#));
        Ok(())
    }

    #[test]
    fn multi_engine_review_builds_one_transaction_per_engine()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempdir()?;
        let mut engines = Vec::new();
        let mut options = Vec::new();
        for version in ["5.7", "5.8"] {
            let engine_path = temp.path().join(format!("UE_{version}"));
            let plugin = engine_path
                .join("Engine")
                .join("Plugins")
                .join("Shared")
                .join("Shared.uplugin");
            fs::create_dir_all(plugin.parent().ok_or("plugin has no parent")?)?;
            fs::write(&plugin, r#"{"EnabledByDefault":true}"#)?;
            engines.push(EngineInstallation {
                path: engine_path,
                version: Some(version.to_owned()),
                source: DiscoverySource::Explicit,
                health: EngineHealth::Healthy,
                descriptor_count: 1,
                issues: Vec::new(),
            });
            options.push(PlanBuildOptions::new(
                temp.path().join(format!("backups-{version}")),
                format!("multi-{version}"),
            )?);
        }
        let document = PresetDocument::parse(
            "schema = 1\nname = \"Shared review\"\nenable = []\ndisable = [\"Shared\"]\nclear = []\ndisable_matching = []\n",
        )?;

        let plans = build_multi_engine_review_with_options(
            &engines,
            &temp.path().join("shared.toml"),
            &document,
            &options,
        )?;

        assert_eq!(plans.len(), 2);
        assert!(
            plans
                .iter()
                .all(|reviewed| reviewed.plan.changes().len() == 1)
        );
        assert_ne!(plans[0].plan.operation_id(), plans[1].plan.operation_id());
        Ok(())
    }
}
