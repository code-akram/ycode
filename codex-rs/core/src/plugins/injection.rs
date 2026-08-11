use codex_protocol::models::ResponseItem;

use crate::context::ContextualUserFragment;
use crate::context::PluginInstructions;
use crate::plugins::PluginCapabilitySummary;
use crate::plugins::render_explicit_plugin_instructions;

pub(crate) fn build_plugin_injections(
    mentioned_plugins: &[PluginCapabilitySummary],
) -> Vec<ResponseItem> {
    if mentioned_plugins.is_empty() {
        return Vec::new();
    }

    // Turn each explicit plugin mention into a developer hint that points the model at the
    // plugin's skill prefix.
    mentioned_plugins
        .iter()
        .filter_map(|plugin| {
            render_explicit_plugin_instructions(plugin)
                .map(PluginInstructions::new)
                .map(ContextualUserFragment::into)
        })
        .collect()
}
