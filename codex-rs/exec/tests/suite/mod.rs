// Aggregates all former standalone integration tests as modules.
mod add_dir;
mod agents_md;
mod apply_patch;
mod auth_env;
#[path = "completion_backfill_tests.rs"]
mod completion_backfill;
mod ephemeral;
mod originator;
mod output_schema;
mod prompt_stdin;
mod resume;
mod server_error_exit;
