//! Direct tool execution without approval or sandbox routing.

use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;

pub(crate) struct ToolOrchestrator;

impl ToolOrchestrator {
    pub fn new() -> Self {
        Self
    }

    pub async fn run<Rq, Out, T>(
        &mut self,
        tool: &mut T,
        req: &Rq,
        tool_ctx: &ToolCtx,
    ) -> Result<Out, ToolError>
    where
        T: ToolRuntime<Rq, Out>,
    {
        tool.run(req, tool_ctx).await
    }
}
