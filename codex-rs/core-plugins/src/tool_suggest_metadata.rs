use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use codex_plugin::PluginCapabilitySummary;
use codex_plugin::PluginId;
use codex_plugin::PluginIdError;
use codex_plugin::prompt_safe_plugin_description;
use codex_protocol::protocol::Product;
use codex_skills::SkillConfigRules;
use codex_utils_plugins::PluginIdentity;
use tokio::sync::Semaphore;

use crate::loader::PluginSkillInventory;
use crate::loader::load_plugin_skill_inventory;
use crate::manager::ConfiguredMarketplacePlugin;
use crate::manager::remote_plugin_install_required_description;
use crate::manifest::load_plugin_manifest_with_format;
use crate::marketplace::MarketplaceError;
use crate::marketplace::MarketplacePluginSource;

const MAX_TOOL_SUGGEST_METADATA_CACHE_ENTRIES: usize = 1024;

type ToolSuggestMetadataEntry = Result<Arc<ToolSuggestMetadataFragment>, String>;

/// Source-derived plugin metadata cached for tool suggestions.
///
/// `PluginsManager` clears these entries alongside its loaded-plugin cache. Current skill config
/// and auth routing are projected after each lookup and are not part of this cache.
pub(crate) struct ToolSuggestMetadataCache {
    state: RwLock<ToolSuggestMetadataCacheState>,
    load_semaphore: Semaphore,
}

#[derive(Default)]
struct ToolSuggestMetadataCacheState {
    generation: u64,
    entries: HashMap<PluginArtifactIdentity, ToolSuggestMetadataEntry>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PluginArtifactIdentity {
    plugin_id: String,
    source: MarketplacePluginSource,
}

pub(crate) struct ToolSuggestMetadataFragment {
    config_name: String,
    display_name: String,
    plugin_namespace: Option<String>,
    description: Option<String>,
    skill_inventory: Option<PluginSkillInventory>,
}

impl ToolSuggestMetadataFragment {
    pub(crate) fn project(
        &self,
        skill_config_rules: &SkillConfigRules,
        _auth_mode: Option<codex_protocol::auth::AuthMode>,
    ) -> PluginCapabilitySummary {
        PluginCapabilitySummary {
            config_name: self.config_name.clone(),
            display_name: self.display_name.clone(),
            plugin_namespace: self.plugin_namespace.clone(),
            description: self.description.clone(),
            has_skills: self
                .skill_inventory
                .as_ref()
                .is_some_and(|inventory| inventory.has_enabled_skills(skill_config_rules)),
        }
    }
}

impl ToolSuggestMetadataCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(ToolSuggestMetadataCacheState::default()),
            load_semaphore: Semaphore::new(/*permits*/ 1),
        }
    }

    pub(crate) fn clear(&self) {
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(err) => err.into_inner(),
        };
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
    }

    pub(crate) async fn metadata_for_plugin(
        &self,
        marketplace_name: &str,
        plugin: &ConfiguredMarketplacePlugin,
        restriction_product: Option<Product>,
        root_scan_slots: Arc<Semaphore>,
    ) -> Result<Arc<ToolSuggestMetadataFragment>, MarketplaceError> {
        let artifact = PluginArtifactIdentity {
            plugin_id: plugin.id.clone(),
            source: plugin.source.clone(),
        };
        loop {
            if let Some(entry) = self.cached_entry(&artifact) {
                return entry.map_err(MarketplaceError::InvalidPlugin);
            }

            let _load_permit = self.load_semaphore.acquire().await.map_err(|_| {
                MarketplaceError::InvalidPlugin(
                    "tool-suggest metadata cache loader closed".to_string(),
                )
            })?;
            if let Some(entry) = self.cached_entry(&artifact) {
                return entry.map_err(MarketplaceError::InvalidPlugin);
            }

            let generation = self.generation();
            let entry = load_plugin_metadata(
                marketplace_name,
                plugin,
                restriction_product,
                Arc::clone(&root_scan_slots),
            )
            .await;
            if self.cache_entry_if_current(generation, artifact.clone(), entry.clone()) {
                return entry.map_err(MarketplaceError::InvalidPlugin);
            }
        }
    }

    fn cached_entry(&self, artifact: &PluginArtifactIdentity) -> Option<ToolSuggestMetadataEntry> {
        match self.state.read() {
            Ok(state) => state.entries.get(artifact).cloned(),
            Err(err) => err.into_inner().entries.get(artifact).cloned(),
        }
    }

    fn generation(&self) -> u64 {
        match self.state.read() {
            Ok(state) => state.generation,
            Err(err) => err.into_inner().generation,
        }
    }

    fn cache_entry_if_current(
        &self,
        generation: u64,
        artifact: PluginArtifactIdentity,
        entry: ToolSuggestMetadataEntry,
    ) -> bool {
        let mut state = match self.state.write() {
            Ok(state) => state,
            Err(err) => err.into_inner(),
        };
        if state.generation != generation {
            return false;
        }
        if state.entries.len() >= MAX_TOOL_SUGGEST_METADATA_CACHE_ENTRIES
            && !state.entries.contains_key(&artifact)
        {
            state.entries.clear();
        }
        state.entries.insert(artifact, entry);
        true
    }
}

async fn load_plugin_metadata(
    marketplace_name: &str,
    plugin: &ConfiguredMarketplacePlugin,
    restriction_product: Option<Product>,
    root_scan_slots: Arc<Semaphore>,
) -> ToolSuggestMetadataEntry {
    let plugin_id = PluginId::new(plugin.name.clone(), marketplace_name.to_string()).map_err(
        |err| match err {
            PluginIdError::Invalid(message) => message,
        },
    )?;

    let MarketplacePluginSource::Local { path: plugin_root } = &plugin.source else {
        return Ok(Arc::new(ToolSuggestMetadataFragment {
            config_name: plugin.id.clone(),
            display_name: plugin.name.clone(),
            plugin_namespace: Some(plugin.name.clone()),
            description: prompt_safe_plugin_description(Some(
                &remote_plugin_install_required_description(&plugin.source),
            )),
            skill_inventory: None,
        }));
    };
    if !plugin_root.as_path().is_dir() {
        return Err("path does not exist or is not a directory".to_string());
    }
    let loaded_manifest = load_plugin_manifest_with_format(plugin_root.as_path())
        .ok_or_else(|| "missing or invalid plugin.json".to_string())?;
    let plugin_identity = PluginIdentity {
        plugin_id: plugin_id.as_key(),
        remote_plugin_id: None,
    };
    let manifest = loaded_manifest.manifest;
    let skill_inventory = load_plugin_skill_inventory(
        plugin_root,
        &plugin_identity,
        &manifest,
        loaded_manifest.format,
        restriction_product,
        /*plugin_skill_snapshots*/ None,
        root_scan_slots,
    )
    .await;
    Ok(Arc::new(ToolSuggestMetadataFragment {
        config_name: plugin.id.clone(),
        display_name: plugin.name.clone(),
        plugin_namespace: Some(manifest.name.clone()),
        description: prompt_safe_plugin_description(manifest.description.as_deref()),
        skill_inventory: Some(skill_inventory),
    }))
}
