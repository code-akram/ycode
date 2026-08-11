use crate::JsonSchema;
use crate::TS;
use codex_protocol::protocol::SkillDependencies as CoreSkillDependencies;
use codex_protocol::protocol::SkillInterface as CoreSkillInterface;
use codex_protocol::protocol::SkillMetadata as CoreSkillMetadata;
use codex_protocol::protocol::SkillScope as CoreSkillScope;
use codex_protocol::protocol::SkillToolDependency as CoreSkillToolDependency;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsListParams {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cwds: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force_reload: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsListResponse {
    pub data: Vec<SkillsListEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsExtraRootsSetParams {
    pub extra_roots: Vec<AbsolutePathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsExtraRootsSetResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case", export_to = "v2/")]
pub enum SkillScope {
    User,
    Repo,
    System,
    Admin,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub short_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub interface: Option<SkillInterface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub dependencies: Option<SkillDependencies>,
    pub path: AbsolutePathBuf,
    pub scope: SkillScope,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillInterface {
    #[ts(optional)]
    pub display_name: Option<String>,
    #[ts(optional)]
    pub short_description: Option<String>,
    #[ts(optional)]
    pub icon_small: Option<AbsolutePathBuf>,
    #[ts(optional)]
    pub icon_large: Option<AbsolutePathBuf>,
    #[ts(optional)]
    pub brand_color: Option<String>,
    #[ts(optional)]
    pub default_prompt: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillDependencies {
    pub tools: Vec<SkillToolDependency>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillToolDependency {
    #[serde(rename = "type")]
    #[ts(rename = "type")]
    pub r#type: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillErrorInfo {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsListEntry {
    pub cwd: PathBuf,
    pub skills: Vec<SkillMetadata>,
    pub errors: Vec<SkillErrorInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsConfigWriteParams {
    #[ts(optional = nullable)]
    pub path: Option<AbsolutePathBuf>,
    #[ts(optional = nullable)]
    pub name: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsConfigWriteResponse {
    pub effective_enabled: bool,
}

impl From<CoreSkillMetadata> for SkillMetadata {
    fn from(value: CoreSkillMetadata) -> Self {
        Self {
            name: value.name,
            description: value.description,
            short_description: value.short_description,
            interface: value.interface.map(SkillInterface::from),
            dependencies: value.dependencies.map(SkillDependencies::from),
            path: value.path,
            scope: value.scope.into(),
            enabled: true,
        }
    }
}

impl From<CoreSkillInterface> for SkillInterface {
    fn from(value: CoreSkillInterface) -> Self {
        Self {
            display_name: value.display_name,
            short_description: value.short_description,
            brand_color: value.brand_color,
            default_prompt: value.default_prompt,
            icon_small: value.icon_small,
            icon_large: value.icon_large,
        }
    }
}

impl From<CoreSkillDependencies> for SkillDependencies {
    fn from(value: CoreSkillDependencies) -> Self {
        Self {
            tools: value
                .tools
                .into_iter()
                .map(SkillToolDependency::from)
                .collect(),
        }
    }
}

impl From<CoreSkillToolDependency> for SkillToolDependency {
    fn from(value: CoreSkillToolDependency) -> Self {
        Self {
            r#type: value.r#type,
            value: value.value,
            description: value.description,
            transport: value.transport,
            command: value.command,
            url: value.url,
        }
    }
}

impl From<CoreSkillScope> for SkillScope {
    fn from(value: CoreSkillScope) -> Self {
        match value {
            CoreSkillScope::User => Self::User,
            CoreSkillScope::Repo => Self::Repo,
            CoreSkillScope::System => Self::System,
            CoreSkillScope::Admin => Self::Admin,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SkillsChangedNotification {}
