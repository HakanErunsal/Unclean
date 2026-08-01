//! Resolves effective plugin state and records the dependency chain behind each result.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Serialize;

use crate::descriptors::{DeclaredPluginState, PluginDescriptor};

/// Identifies one stable dependency-analysis warning category.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyWarningCode {
    /// Reports more than one descriptor with the same Unreal plugin name.
    DuplicatePluginName,
    /// Reports an enabled reference whose plugin is absent.
    MissingReference,
    /// Reports an enabled reference whose plugin name resolves to multiple descriptors.
    AmbiguousReference,
}

impl DependencyWarningCode {
    /// Returns the stable lowercase identifier used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicatePluginName => "duplicate_plugin_name",
            Self::MissingReference => "missing_reference",
            Self::AmbiguousReference => "ambiguous_reference",
        }
    }
}

/// Reports one graph condition that did not stop dependency analysis.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DependencyWarning {
    /// Identifies the warning category.
    pub code: DependencyWarningCode,
    /// Names the descriptor that owns the reference or duplicate name.
    pub plugin: String,
    /// Names the referenced plugin when the warning concerns one edge.
    pub dependency: Option<String>,
    /// Reports whether Unreal marks the missing reference as optional.
    pub optional: bool,
    /// States the graph condition and recovery action.
    pub message: String,
}

/// Groups analyzed plugins and nonfatal graph warnings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DependencyReport {
    /// Lists plugins with resolved effective state and stable explanations.
    pub plugins: Vec<PluginDescriptor>,
    /// Lists missing, duplicate, and ambiguous graph references.
    pub warnings: Vec<DependencyWarning>,
}

pub(crate) struct EffectiveStatePolicy {
    roots: BTreeSet<String>,
    blocked: BTreeSet<String>,
}

impl EffectiveStatePolicy {
    pub(crate) fn new(
        roots: impl IntoIterator<Item = String>,
        blocked: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            roots: roots
                .into_iter()
                .map(|name| normalized_name(&name))
                .collect(),
            blocked: blocked
                .into_iter()
                .map(|name| normalized_name(&name))
                .collect(),
        }
    }
}

pub(crate) struct BlockedDependency {
    pub(crate) plugin: String,
    pub(crate) dependency: String,
    pub(crate) optional: bool,
}

#[derive(Clone, Copy)]
struct DependencyEdge {
    target: usize,
    optional: bool,
}

/// Computes effective state from default-enabled roots and enabled descriptor references.
#[must_use]
pub fn analyze_plugins(plugins: Vec<PluginDescriptor>) -> DependencyReport {
    let roots = plugins
        .iter()
        .filter(|plugin| plugin.declared_state == DeclaredPluginState::Enabled)
        .map(|plugin| plugin.name.clone())
        .collect::<Vec<_>>();
    let policy = EffectiveStatePolicy::new(roots, std::iter::empty());
    analyze_plugins_with_policy(plugins, &policy).0
}

pub(crate) fn analyze_plugins_with_policy(
    mut plugins: Vec<PluginDescriptor>,
    policy: &EffectiveStatePolicy,
) -> (DependencyReport, Vec<BlockedDependency>) {
    plugins.sort_by(|left, right| {
        normalized_name(&left.name)
            .cmp(&normalized_name(&right.name))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
    let index = build_index(&plugins);
    let mut warnings = duplicate_name_warnings(&plugins, &index);
    let edges = build_edges(&plugins, &index, &mut warnings);
    let blocked_dependencies = resolve_effective_state(&mut plugins, &edges, policy);
    warnings.sort_by(|left, right| {
        normalized_name(&left.plugin)
            .cmp(&normalized_name(&right.plugin))
            .then_with(|| left.code.as_str().cmp(right.code.as_str()))
            .then_with(|| left.dependency.cmp(&right.dependency))
    });
    (DependencyReport { plugins, warnings }, blocked_dependencies)
}

fn normalized_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn build_index(plugins: &[PluginDescriptor]) -> BTreeMap<String, Vec<usize>> {
    let mut index = BTreeMap::<String, Vec<usize>>::new();
    for (plugin_index, plugin) in plugins.iter().enumerate() {
        index
            .entry(normalized_name(&plugin.name))
            .or_default()
            .push(plugin_index);
    }
    index
}

fn duplicate_name_warnings(
    plugins: &[PluginDescriptor],
    index: &BTreeMap<String, Vec<usize>>,
) -> Vec<DependencyWarning> {
    index
        .values()
        .filter(|matches| matches.len() > 1)
        .map(|matches| {
            let plugin = plugins[matches[0]].name.clone();
            let paths = matches
                .iter()
                .map(|index| plugins[*index].path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            DependencyWarning {
                code: DependencyWarningCode::DuplicatePluginName,
                plugin: plugin.clone(),
                dependency: None,
                optional: false,
                message: format!(
                    "Plugin name is ambiguous: {plugin} appears at {paths}. Remove or rename duplicate descriptors before applying changes."
                ),
            }
        })
        .collect()
}

fn build_edges(
    plugins: &[PluginDescriptor],
    index: &BTreeMap<String, Vec<usize>>,
    warnings: &mut Vec<DependencyWarning>,
) -> Vec<Vec<DependencyEdge>> {
    let mut edges = vec![Vec::new(); plugins.len()];
    for (plugin_index, plugin) in plugins.iter().enumerate() {
        for dependency in &plugin.enabled_dependencies {
            match index.get(&normalized_name(&dependency.name)) {
                Some(matches) if matches.len() == 1 => {
                    edges[plugin_index].push(DependencyEdge {
                        target: matches[0],
                        optional: dependency.optional,
                    });
                }
                Some(_) => warnings.push(DependencyWarning {
                    code: DependencyWarningCode::AmbiguousReference,
                    plugin: plugin.name.clone(),
                    dependency: Some(dependency.name.clone()),
                    optional: dependency.optional,
                    message: format!(
                        "Dependency is ambiguous: {} references {}, which matches multiple descriptors. Remove or rename duplicate descriptors before applying changes.",
                        plugin.name, dependency.name
                    ),
                }),
                None => warnings.push(missing_reference_warning(plugin, dependency)),
            }
        }
        edges[plugin_index].sort_by(|left, right| {
            normalized_name(&plugins[left.target].name)
                .cmp(&normalized_name(&plugins[right.target].name))
                .then_with(|| plugins[left.target].name.cmp(&plugins[right.target].name))
                .then_with(|| left.optional.cmp(&right.optional))
        });
        edges[plugin_index].dedup_by_key(|edge| edge.target);
    }
    edges
}

fn missing_reference_warning(
    plugin: &PluginDescriptor,
    dependency: &crate::descriptors::PluginDependencyReference,
) -> DependencyWarning {
    let message = if dependency.optional {
        format!(
            "Plugin {} references missing optional dependency {}. Install the dependency when the optional feature is required.",
            plugin.name, dependency.name
        )
    } else {
        format!(
            "Dependency is missing: {} references {}. Install the plugin or remove the enabled reference.",
            plugin.name, dependency.name
        )
    };
    DependencyWarning {
        code: DependencyWarningCode::MissingReference,
        plugin: plugin.name.clone(),
        dependency: Some(dependency.name.clone()),
        optional: dependency.optional,
        message,
    }
}

fn resolve_effective_state(
    plugins: &mut [PluginDescriptor],
    edges: &[Vec<DependencyEdge>],
    policy: &EffectiveStatePolicy,
) -> Vec<BlockedDependency> {
    let mut queue = VecDeque::new();
    for (index, plugin) in plugins.iter_mut().enumerate() {
        let root = policy.roots.contains(&normalized_name(&plugin.name));
        plugin.effective_enabled = Some(root);
        plugin.effective_path.clear();
        plugin.reached_by.clear();
        if root {
            plugin.effective_path.push(plugin.name.clone());
            queue.push_back(index);
        }
    }

    let mut blocked_dependencies = Vec::new();
    let mut blocked_edges = BTreeSet::new();
    while let Some(source) = queue.pop_front() {
        let source_name = plugins[source].name.clone();
        let source_path = plugins[source].effective_path.clone();
        for edge in &edges[source] {
            let target = edge.target;
            if !plugins[target]
                .reached_by
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&source_name))
            {
                plugins[target].reached_by.push(source_name.clone());
            }
            if policy
                .blocked
                .contains(&normalized_name(&plugins[target].name))
            {
                if blocked_edges.insert((source, target)) {
                    blocked_dependencies.push(BlockedDependency {
                        plugin: source_name.clone(),
                        dependency: plugins[target].name.clone(),
                        optional: edge.optional,
                    });
                }
                continue;
            }
            if plugins[target].effective_enabled == Some(true) {
                continue;
            }
            plugins[target].effective_enabled = Some(true);
            plugins[target].effective_path.clone_from(&source_path);
            let target_name = plugins[target].name.clone();
            plugins[target].effective_path.push(target_name);
            queue.push_back(target);
        }
    }

    for plugin in plugins {
        plugin.reached_by.sort_by(|left, right| {
            normalized_name(left)
                .cmp(&normalized_name(right))
                .then_with(|| left.cmp(right))
        });
    }
    blocked_dependencies.sort_by(|left, right| {
        normalized_name(&left.plugin)
            .cmp(&normalized_name(&right.plugin))
            .then_with(|| {
                normalized_name(&left.dependency).cmp(&normalized_name(&right.dependency))
            })
    });
    blocked_dependencies
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::path::Path;

    use super::{DependencyWarningCode, analyze_plugins};
    use crate::descriptors::DescriptorDocument;

    #[test]
    fn cycles_reach_a_fixed_point_with_stable_paths() -> Result<(), Box<dyn Error>> {
        let plugins = vec![
            descriptor("CycleC", false, &[("CycleA", false)])?,
            descriptor("CycleA", true, &[("CycleB", false)])?,
            descriptor("CycleB", false, &[("CycleC", false)])?,
        ];

        let report = analyze_plugins(plugins);

        assert!(
            report
                .plugins
                .iter()
                .all(|plugin| plugin.effective_enabled == Some(true))
        );
        assert_eq!(
            plugin(&report.plugins, "CycleA")?.effective_path,
            ["CycleA"]
        );
        assert_eq!(
            plugin(&report.plugins, "CycleB")?.effective_path,
            ["CycleA", "CycleB"]
        );
        assert_eq!(
            plugin(&report.plugins, "CycleC")?.effective_path,
            ["CycleA", "CycleB", "CycleC"]
        );
        assert_eq!(plugin(&report.plugins, "CycleA")?.reached_by, ["CycleC"]);
        Ok(())
    }

    #[test]
    fn missing_references_warn_without_stopping_the_closure() -> Result<(), Box<dyn Error>> {
        let plugins = vec![
            descriptor(
                "Root",
                true,
                &[("RequiredMissing", false), ("OptionalMissing", true)],
            )?,
            descriptor("Unrelated", false, &[])?,
        ];

        let report = analyze_plugins(plugins);

        assert_eq!(
            plugin(&report.plugins, "Root")?.effective_enabled,
            Some(true)
        );
        assert_eq!(
            plugin(&report.plugins, "Unrelated")?.effective_enabled,
            Some(false)
        );
        assert_eq!(report.warnings.len(), 2);
        assert!(
            report
                .warnings
                .iter()
                .all(|warning| warning.code == DependencyWarningCode::MissingReference)
        );
        assert!(report.warnings.iter().any(|warning| warning.optional));
        Ok(())
    }

    #[test]
    fn lexical_root_order_selects_the_stable_shortest_explanation() -> Result<(), Box<dyn Error>> {
        let plugins = vec![
            descriptor("ZuluRoot", true, &[("Shared", false)])?,
            descriptor("AlphaRoot", true, &[("Shared", false)])?,
            descriptor("Shared", false, &[])?,
        ];

        let report = analyze_plugins(plugins);

        assert_eq!(
            plugin(&report.plugins, "Shared")?.effective_path,
            ["AlphaRoot", "Shared"]
        );
        assert_eq!(
            plugin(&report.plugins, "Shared")?.reached_by,
            ["AlphaRoot", "ZuluRoot"]
        );
        Ok(())
    }

    fn descriptor(
        name: &str,
        enabled: bool,
        dependencies: &[(&str, bool)],
    ) -> Result<crate::descriptors::PluginDescriptor, Box<dyn Error>> {
        let references = dependencies
            .iter()
            .map(|(dependency, optional)| {
                format!("{{\"Name\":\"{dependency}\",\"Enabled\":true,\"Optional\":{optional}}}")
            })
            .collect::<Vec<_>>()
            .join(",");
        let source = format!("{{\"EnabledByDefault\":{enabled},\"Plugins\":[{references}]}}");
        let document = DescriptorDocument::parse(source.as_bytes())?;
        let file_name = format!("{name}.uplugin");
        Ok(document.plugin_descriptor(Path::new(&file_name), Path::new("")))
    }

    fn plugin<'a>(
        plugins: &'a [crate::descriptors::PluginDescriptor],
        name: &str,
    ) -> Result<&'a crate::descriptors::PluginDescriptor, Box<dyn Error>> {
        plugins
            .iter()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| format!("plugin {name} is missing").into())
    }
}
