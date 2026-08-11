use std::collections::HashSet;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_plugins::PluginIdentity;
use codex_utils_plugins::PluginSkillRoot;
use codex_utils_plugins::SkillDiscoveryMode;

use crate::PluginCapabilitySummary;
use crate::PluginHookSource;

const MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN: usize = 1024;

/// A plugin that was loaded from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPlugin {
    pub config_name: String,
    pub remote_plugin_id: Option<String>,
    pub manifest_name: Option<String>,
    pub plugin_namespace: Option<String>,
    pub manifest_description: Option<String>,
    pub root: AbsolutePathBuf,
    pub enabled: bool,
    pub skill_roots: Vec<AbsolutePathBuf>,
    pub skill_discovery_mode: SkillDiscoveryMode,
    pub disabled_skill_paths: HashSet<AbsolutePathBuf>,
    pub has_enabled_skills: bool,
    pub hook_sources: Vec<PluginHookSource>,
    pub hook_load_warnings: Vec<String>,
    pub error: Option<String>,
}

impl LoadedPlugin {
    pub fn is_active(&self) -> bool {
        self.enabled && self.error.is_none()
    }

    pub fn display_name(&self) -> &str {
        self.manifest_name.as_deref().unwrap_or(&self.config_name)
    }

    pub fn is_agent_plugin(&self) -> bool {
        self.skill_discovery_mode == SkillDiscoveryMode::DirectChildren
    }
}

fn plugin_capability_summary_from_loaded(plugin: &LoadedPlugin) -> Option<PluginCapabilitySummary> {
    if !plugin.is_active() {
        return None;
    }

    let summary = PluginCapabilitySummary {
        config_name: plugin.config_name.clone(),
        display_name: plugin.display_name().to_string(),
        plugin_namespace: plugin.plugin_namespace.clone(),
        description: prompt_safe_plugin_description(plugin.manifest_description.as_deref()),
        has_skills: plugin.has_enabled_skills,
    };

    summary.has_skills.then_some(summary)
}

/// Normalizes plugin descriptions for inclusion in model-facing capability summaries.
pub fn prompt_safe_plugin_description(description: Option<&str>) -> Option<String> {
    let description = description?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if description.is_empty() {
        return None;
    }

    Some(
        description
            .chars()
            .take(MAX_CAPABILITY_SUMMARY_DESCRIPTION_LEN)
            .collect(),
    )
}

/// Runtime view of loaded plugins and their derived capability summaries.
///
/// Callers must apply any runtime capability policies before constructing this outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginLoadOutcome {
    plugins: Vec<LoadedPlugin>,
    capability_summaries: Vec<PluginCapabilitySummary>,
}

impl Default for PluginLoadOutcome {
    fn default() -> Self {
        Self::from_plugins(Vec::new())
    }
}

impl PluginLoadOutcome {
    pub fn from_plugins(plugins: Vec<LoadedPlugin>) -> Self {
        let capability_summaries = plugins
            .iter()
            .filter_map(plugin_capability_summary_from_loaded)
            .collect::<Vec<_>>();
        Self {
            plugins,
            capability_summaries,
        }
    }

    pub fn effective_skill_roots(&self) -> Vec<AbsolutePathBuf> {
        let mut skill_roots: Vec<AbsolutePathBuf> = self
            .plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.skill_roots.iter().cloned())
            .collect();
        skill_roots.sort_unstable();
        skill_roots.dedup();
        skill_roots
    }

    pub fn effective_plugin_skill_roots(&self) -> Vec<PluginSkillRoot> {
        let mut skill_roots = Vec::new();
        let mut seen_paths = HashSet::new();
        for plugin in self.plugins.iter().filter(|plugin| plugin.is_active()) {
            let Some(plugin_namespace) = &plugin.plugin_namespace else {
                continue;
            };
            for path in &plugin.skill_roots {
                if seen_paths.insert(path.clone()) {
                    skill_roots.push(PluginSkillRoot {
                        path: path.clone(),
                        plugin_identity: PluginIdentity {
                            plugin_id: plugin.config_name.clone(),
                            remote_plugin_id: plugin.remote_plugin_id.clone(),
                        },
                        plugin_namespace: plugin_namespace.clone(),
                        plugin_root: plugin.root.clone(),
                        discovery_mode: plugin.skill_discovery_mode,
                    });
                }
            }
        }

        skill_roots.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        skill_roots
    }

    pub fn effective_plugin_hook_sources(&self) -> Vec<PluginHookSource> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_sources.iter().cloned())
            .collect()
    }

    pub fn effective_plugin_hook_warnings(&self) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.is_active())
            .flat_map(|plugin| plugin.hook_load_warnings.iter().cloned())
            .collect()
    }

    pub fn capability_summaries(&self) -> &[PluginCapabilitySummary] {
        &self.capability_summaries
    }

    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }
}

/// Implemented by [`PluginLoadOutcome`] so callers can depend on skill roots only.
pub trait EffectiveSkillRoots {
    fn effective_skill_roots(&self) -> Vec<AbsolutePathBuf>;

    fn effective_plugin_skill_roots(&self) -> Vec<PluginSkillRoot>;
}

impl EffectiveSkillRoots for PluginLoadOutcome {
    fn effective_skill_roots(&self) -> Vec<AbsolutePathBuf> {
        PluginLoadOutcome::effective_skill_roots(self)
    }

    fn effective_plugin_skill_roots(&self) -> Vec<PluginSkillRoot> {
        PluginLoadOutcome::effective_plugin_skill_roots(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> AbsolutePathBuf {
        AbsolutePathBuf::from_absolute_path_checked(std::env::temp_dir().join(name))
            .expect("absolute temp path")
    }

    fn loaded_plugin(config_name: &str, skill_roots: Vec<AbsolutePathBuf>) -> LoadedPlugin {
        LoadedPlugin {
            config_name: config_name.to_string(),
            remote_plugin_id: None,
            manifest_name: None,
            plugin_namespace: Some(
                config_name
                    .split_once('@')
                    .map_or(config_name, |(name, _)| name)
                    .to_string(),
            ),
            manifest_description: None,
            root: test_path(config_name),
            enabled: true,
            skill_roots,
            skill_discovery_mode: SkillDiscoveryMode::Recursive,
            disabled_skill_paths: HashSet::new(),
            has_enabled_skills: true,
            hook_sources: Vec::new(),
            hook_load_warnings: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn effective_plugin_skill_roots_preserves_first_plugin_for_shared_root() {
        let shared_root = test_path("shared-skills");
        let mut first_plugin = loaded_plugin("zeta@test", vec![shared_root.clone()]);
        first_plugin.remote_plugin_id = Some("plugins~Plugin_zeta".to_string());
        let outcome = PluginLoadOutcome::from_plugins(vec![
            first_plugin,
            loaded_plugin("alpha@test", vec![shared_root.clone()]),
        ]);

        assert_eq!(
            outcome.effective_plugin_skill_roots(),
            vec![PluginSkillRoot {
                path: shared_root,
                plugin_identity: PluginIdentity {
                    plugin_id: "zeta@test".to_string(),
                    remote_plugin_id: Some("plugins~Plugin_zeta".to_string()),
                },
                plugin_namespace: "zeta".to_string(),
                plugin_root: test_path("zeta@test"),
                discovery_mode: SkillDiscoveryMode::Recursive,
            }]
        );
    }
}
