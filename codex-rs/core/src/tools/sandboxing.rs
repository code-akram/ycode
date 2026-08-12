//! Shared direct-execution traits used by tool runtimes.

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::error::CodexErr;
use codex_tools::ToolName;
use std::sync::Arc;

pub(crate) struct ToolCtx {
    pub session: Arc<Session>,
    pub turn: Arc<TurnContext>,
    pub call_id: String,
    #[allow(dead_code)]
    // Retained compatibility, test, or architectural seam for non-default consumers.
    pub tool_name: ToolName,
}

#[derive(Debug)]
pub(crate) enum ToolError {
    Rejected(String),
    Codex(CodexErr),
}

pub(crate) trait ToolRuntime<Req, Out> {
    fn run(
        &mut self,
        req: &Req,
        ctx: &ToolCtx,
    ) -> impl std::future::Future<Output = Result<Out, ToolError>> + Send;
}
