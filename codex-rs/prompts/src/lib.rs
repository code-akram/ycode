mod apply_patch;
mod compact;
mod goals;
mod realtime;

pub use apply_patch::APPLY_PATCH_TOOL_INSTRUCTIONS;
pub use compact::SUMMARIZATION_PROMPT;
pub use compact::SUMMARY_PREFIX;
pub use goals::budget_limit_prompt;
pub use goals::continuation_prompt;
pub use goals::objective_updated_prompt;
pub use realtime::BACKEND_PROMPT;
pub use realtime::END_INSTRUCTIONS;
pub use realtime::START_INSTRUCTIONS;
