use serde::Deserialize;
use serde::Serialize;

pub const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
pub const TOOL_SEARCH_DEFAULT_LIMIT: usize = 8;
pub const LIST_AVAILABLE_PLUGINS_TO_INSTALL_TOOL_NAME: &str = "list_available_plugins_to_install";
pub const REQUEST_PLUGIN_INSTALL_TOOL_NAME: &str = "request_plugin_install";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSearchSourceInfo {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverableToolType {
    Plugin,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverableToolAction {
    Install,
    Enable,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DiscoverableTool {
    Plugin(Box<DiscoverablePluginInfo>),
}

impl DiscoverableTool {
    pub fn tool_type(&self) -> DiscoverableToolType {
        match self {
            Self::Plugin(_) => DiscoverableToolType::Plugin,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Plugin(plugin) => plugin.id.as_str(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Plugin(plugin) => plugin.name.as_str(),
        }
    }
}

impl From<DiscoverablePluginInfo> for DiscoverableTool {
    fn from(value: DiscoverablePluginInfo) -> Self {
        Self::Plugin(Box::new(value))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoverablePluginInfo {
    pub id: String,
    pub remote_plugin_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub has_skills: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RequestPluginInstallEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub tool_type: DiscoverableToolType,
    pub has_skills: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ListAvailablePluginsToInstallResult {
    pub tools: Vec<RequestPluginInstallEntry>,
}

pub fn collect_request_plugin_install_entries(
    discoverable_tools: &[DiscoverableTool],
) -> Vec<RequestPluginInstallEntry> {
    discoverable_tools
        .iter()
        .map(|tool| match tool {
            DiscoverableTool::Plugin(plugin) => RequestPluginInstallEntry {
                id: plugin.id.clone(),
                name: plugin.name.clone(),
                description: plugin.description.clone(),
                tool_type: DiscoverableToolType::Plugin,
                has_skills: plugin.has_skills,
            },
        })
        .collect()
}

#[cfg(test)]
#[path = "tool_discovery_tests.rs"]
mod tests;
