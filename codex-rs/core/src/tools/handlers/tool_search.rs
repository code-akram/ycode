use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::ToolSearchOutput;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::tool_search_spec::ToolSearchSourceListing;
use crate::tools::handlers::tool_search_spec::create_tool_search_tool;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use bm25::Document;
use bm25::Language;
use bm25::SearchEngine;
use bm25::SearchEngineBuilder;
use codex_tools::LoadableToolSpec;
use codex_tools::TOOL_SEARCH_DEFAULT_LIMIT;
use codex_tools::TOOL_SEARCH_TOOL_NAME;
use codex_tools::ToolName;
use codex_tools::ToolSearchEntry;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use codex_tools::coalesce_loadable_tool_specs;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::instrument;

pub struct ToolSearchHandler {
    search_infos: Vec<ToolSearchInfo>,
    source_listing: ToolSearchSourceListing,
    spec: ToolSpec,
    search_engine: SearchEngine<usize>,
}

#[derive(Default)]
pub(crate) struct ToolSearchHandlerCache {
    cached: Mutex<Option<Arc<ToolSearchHandler>>>,
}

impl ToolSearchHandlerCache {
    #[instrument(level = "trace", skip_all, fields(search_info_count = search_infos.len()))]
    pub(crate) fn get_or_build(
        &self,
        search_infos: Vec<ToolSearchInfo>,
        source_listing: ToolSearchSourceListing,
    ) -> Arc<ToolSearchHandler> {
        {
            let cached = self.cached();
            if let Some(cached) = cached.as_ref()
                && cached.search_infos == search_infos
                && cached.source_listing == source_listing
            {
                return Arc::clone(cached);
            }
        }

        let handler = Arc::new(ToolSearchHandler::new(search_infos, source_listing));
        let mut cached = self.cached();
        if let Some(cached) = cached.as_ref()
            && cached.search_infos == handler.search_infos
            && cached.source_listing == handler.source_listing
        {
            return Arc::clone(cached);
        }

        *cached = Some(Arc::clone(&handler));
        handler
    }

    fn cached(&self) -> std::sync::MutexGuard<'_, Option<Arc<ToolSearchHandler>>> {
        match self.cached.lock() {
            Ok(cached) => cached,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl ToolSearchHandler {
    #[instrument(
        level = "trace",
        skip_all,
        fields(search_info_count = search_infos.len())
    )]
    pub(crate) fn new(
        search_infos: Vec<ToolSearchInfo>,
        source_listing: ToolSearchSourceListing,
    ) -> Self {
        let search_source_infos = search_infos
            .iter()
            .filter_map(|search_info| search_info.source_info.clone())
            .collect::<Vec<_>>();
        let spec = create_tool_search_tool(
            &search_source_infos,
            TOOL_SEARCH_DEFAULT_LIMIT,
            source_listing,
        );
        let documents: Vec<Document<usize>> = search_infos
            .iter()
            .map(|search_info| search_info.entry.search_text.clone())
            .enumerate()
            .map(|(idx, search_text)| Document::new(idx, search_text))
            .collect();
        let search_engine =
            SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build();

        Self {
            search_infos,
            source_listing,
            spec,
            search_engine,
        }
    }
}

impl ToolExecutor<ToolInvocation> for ToolSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ToolSearchHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation { payload, .. } = invocation;

        let args = match payload {
            ToolPayload::ToolSearch { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::Fatal(format!(
                    "{TOOL_SEARCH_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        let query = args.query.trim();
        if query.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "query must not be empty".to_string(),
            ));
        }
        let limit = args.limit.unwrap_or(TOOL_SEARCH_DEFAULT_LIMIT);

        if limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "limit must be greater than zero".to_string(),
            ));
        }

        if self.search_infos.is_empty() {
            return Ok(boxed_tool_output(ToolSearchOutput { tools: Vec::new() }));
        }

        let tools = self.search(query, limit)?;

        Ok(boxed_tool_output(ToolSearchOutput { tools }))
    }
}

impl CoreToolRuntime for ToolSearchHandler {}

impl ToolSearchHandler {
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LoadableToolSpec>, FunctionCallError> {
        let results = self
            .search_engine
            .search(query, limit)
            .into_iter()
            .map(|result| result.document.id)
            .filter_map(|id| self.search_infos.get(id))
            .map(|search_info| &search_info.entry);
        self.search_output_tools(results)
    }

    fn search_output_tools<'a>(
        &self,
        results: impl IntoIterator<Item = &'a ToolSearchEntry>,
    ) -> Result<Vec<LoadableToolSpec>, FunctionCallError> {
        Ok(coalesce_loadable_tool_specs(
            results.into_iter().map(|entry| entry.output.clone()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::handlers::DynamicToolHandler;
    use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
    use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
    use std::sync::Arc;

    #[test]
    fn cache_reuses_handler_for_identical_search_infos_and_rebuilds_for_changes() {
        let cache = ToolSearchHandlerCache::default();
        let namespace = DynamicToolNamespaceSpec {
            name: "calendar".to_string(),
            description: "Calendar tools".to_string(),
            tools: Vec::new(),
        };
        let tool = DynamicToolFunctionSpec {
            name: "create_event".to_string(),
            description: "Create events".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
            defer_loading: true,
        };
        let search_infos = vec![
            DynamicToolHandler::new_in_namespace(&namespace, &tool)
                .expect("dynamic tool should convert")
                .search_info()
                .expect("dynamic handler should return search info"),
        ];

        let first = cache.get_or_build(search_infos.clone(), ToolSearchSourceListing::Include);
        let second = cache.get_or_build(search_infos.clone(), ToolSearchSourceListing::Include);
        assert!(Arc::ptr_eq(&first, &second));

        let without_sources =
            cache.get_or_build(search_infos.clone(), ToolSearchSourceListing::Omit);
        assert!(!Arc::ptr_eq(&first, &without_sources));

        let mut changed_search_infos = search_infos;
        changed_search_infos[0]
            .entry
            .search_text
            .push_str(" changed");
        let changed = cache.get_or_build(changed_search_infos, ToolSearchSourceListing::Omit);
        assert!(!Arc::ptr_eq(&first, &changed));
    }
}
