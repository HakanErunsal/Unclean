//! Resolves portable preset rules into project descriptor edits.

use serde::Serialize;

use crate::Result;
use crate::presets::{Preset, PresetAction, PresetResolution};
use crate::projects::{
    ProjectDescriptorEdit, ProjectPluginEdit, ProjectPluginEditAction, ProjectSuppressionEdit,
};

/// Contains the reviewed preset resolution and its project descriptor edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPresetResolution {
    /// Retains exact matches, pattern expansions, and unmatched rules.
    pub preset: PresetResolution,
    /// Maps the resolved rules and selected suppression change to `.uproject` fields.
    pub edit: ProjectDescriptorEdit,
}

/// Resolves one preset against engine plugin names and maps it to project fields.
///
/// # Errors
///
/// Returns an error when plugin names are ambiguous or preset rules conflict during resolution.
pub fn resolve_project_preset(
    preset: &Preset,
    plugin_names: &[String],
    suppression: ProjectSuppressionEdit,
) -> Result<ProjectPresetResolution> {
    let resolution = preset.resolve(plugin_names)?;
    let plugins = resolution
        .changes
        .iter()
        .map(|change| ProjectPluginEdit {
            plugin: change.plugin.clone(),
            action: project_action(change.action),
        })
        .collect();
    Ok(ProjectPresetResolution {
        edit: ProjectDescriptorEdit {
            suppression,
            plugins,
        },
        preset: resolution,
    })
}

const fn project_action(action: PresetAction) -> ProjectPluginEditAction {
    match action {
        PresetAction::Enable => ProjectPluginEditAction::Enable,
        PresetAction::Disable => ProjectPluginEditAction::Disable,
        PresetAction::Clear => ProjectPluginEditAction::Clear,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::resolve_project_preset;
    use crate::presets::PresetDocument;
    use crate::projects::{
        ProjectDescriptorDocument, ProjectPluginEditAction, ProjectSuppressionEdit,
        ProjectSuppressionState,
    };

    #[test]
    fn preset_actions_map_to_explicit_project_references() -> Result<(), Box<dyn Error>> {
        let preset = PresetDocument::parse(
            r#"
schema = 1
name = "Synthetic project preset"
enable = ["ExplicitOn"]
disable = ["ExplicitOff"]
clear = ["RemoveOverride"]
disable_matching = ["Android*"]
"#,
        )?;
        let plugin_names = [
            "ExplicitOn",
            "ExplicitOff",
            "RemoveOverride",
            "AndroidRuntime",
            "Unchanged",
        ]
        .map(str::to_owned);

        let resolved = resolve_project_preset(
            preset.preset(),
            &plugin_names,
            ProjectSuppressionEdit::Set(true),
        )?;

        assert_eq!(resolved.edit.suppression, ProjectSuppressionEdit::Set(true));
        assert!(resolved.edit.plugins.iter().any(|edit| {
            edit.plugin == "ExplicitOn" && edit.action == ProjectPluginEditAction::Enable
        }));
        assert!(resolved.edit.plugins.iter().any(|edit| {
            edit.plugin == "ExplicitOff" && edit.action == ProjectPluginEditAction::Disable
        }));
        assert!(resolved.edit.plugins.iter().any(|edit| {
            edit.plugin == "RemoveOverride" && edit.action == ProjectPluginEditAction::Clear
        }));
        assert!(resolved.edit.plugins.iter().any(|edit| {
            edit.plugin == "AndroidRuntime" && edit.action == ProjectPluginEditAction::Disable
        }));
        assert_eq!(
            resolved.preset.pattern_expansions[0].matches,
            ["AndroidRuntime"]
        );
        Ok(())
    }

    #[test]
    fn resolved_project_preset_edits_the_selected_project_state() -> Result<(), Box<dyn Error>> {
        let preset = PresetDocument::parse(
            r#"
schema = 1
name = "Synthetic project preset"
enable = ["EngineOff"]
disable = ["EngineDefault"]
clear = ["OldOverride"]
disable_matching = []
"#,
        )?;
        let project = ProjectDescriptorDocument::parse(
            br#"{
                "DisableEnginePluginsByDefault": false,
                "Plugins": [{"Name":"OldOverride","Enabled":false}]
            }"#,
        )?;
        let plugin_names = ["EngineOff", "EngineDefault", "OldOverride"].map(str::to_owned);
        let resolved = resolve_project_preset(
            preset.preset(),
            &plugin_names,
            ProjectSuppressionEdit::Set(true),
        )?;

        let edited = project.edit(&resolved.edit)?;
        let verified = ProjectDescriptorDocument::parse(&edited)?;

        assert_eq!(verified.suppression(), ProjectSuppressionState::Enabled);
        assert!(
            verified
                .plugins()
                .iter()
                .any(|plugin| plugin.name == "EngineOff" && plugin.enabled)
        );
        assert!(
            verified
                .plugins()
                .iter()
                .any(|plugin| plugin.name == "EngineDefault" && !plugin.enabled)
        );
        assert!(
            !verified
                .plugins()
                .iter()
                .any(|plugin| plugin.name == "OldOverride")
        );
        Ok(())
    }
}
