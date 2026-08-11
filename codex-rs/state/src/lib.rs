//! SQLite-backed state for rollout metadata.
//!
//! This crate is intentionally small and focused: it extracts rollout metadata
//! from JSONL rollouts and mirrors it into a local SQLite database. Backfill
//! orchestration and rollout scanning live in `codex-core`.

const _: () = assert!(
    libsqlite3_sys::SQLITE_VERSION_NUMBER >= 3_051_003,
    "bundled SQLite must include the WAL-reset corruption fix",
);

mod audit;
mod extract;
pub mod log_db;
mod migrations;
mod model;
mod paths;
mod runtime;
mod sqlite;

pub use model::LogEntry;
pub use model::LogQuery;
pub use model::LogRow;
pub use model::Phase2JobClaimOutcome;
pub use model::QueuedUserSubmissionRecord;
/// Preferred entrypoint: owns configuration and runtime state.
pub use runtime::StateRuntime;
pub use sqlite::SqliteConfig;

pub use audit::ThreadStateAuditRow;
pub use audit::read_thread_state_audit_rows;
/// Low-level storage engine: useful for focused tests.
///
/// Most consumers should prefer [`StateRuntime`].
pub use extract::apply_rollout_item;
pub use extract::rollout_item_affects_thread_metadata;
pub use model::Anchor;
pub use model::BackfillState;
pub use model::BackfillStats;
pub use model::BackfillStatus;
pub use model::DirectionalThreadSpawnEdgeStatus;
pub use model::ExtractionOutcome;
pub use model::SortDirection;
pub use model::SortKey;
pub use model::Stage1JobClaim;
pub use model::Stage1JobClaimOutcome;
pub use model::Stage1Output;
pub use model::Stage1StartupClaimParams;
pub use model::ThreadGoal;
pub use model::ThreadGoalStatus;
pub use model::ThreadMetadata;
pub use model::ThreadMetadataBuilder;
pub use model::ThreadRelationFilter;
pub use model::ThreadSection;
pub use model::ThreadSectionsPage;
pub use model::ThreadsPage;
pub use runtime::GoalAccountingMode;
pub use runtime::GoalAccountingOutcome;
pub use runtime::GoalStore;
pub use runtime::GoalUpdate;
pub use runtime::MemoryStore;
pub use runtime::RuntimeDbBackup;
pub use runtime::SqliteQueueStore;
pub use runtime::ThreadFilterOptions;
pub use runtime::backup_runtime_db_for_fresh_start;
pub use runtime::is_sqlite_corruption_error;
pub use runtime::open_thread_history_db;
pub use runtime::runtime_db_path_for_corruption_error;
pub use runtime::sqlite_error_detail_is_corruption;
pub use runtime::sqlite_error_detail_is_lock;
pub use runtime::sqlite_integrity_check;
pub use sqlite::RuntimeDbPath;

/// Maximum number of pending user submissions permitted for one thread.
pub const MAX_QUEUE_ITEMS: usize = 100;

/// Stable UUIDv7 identifying the built-in pinned thread section.
pub const PINNED_THREAD_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf318";

/// User-facing name of the built-in pinned thread section.
pub const PINNED_THREAD_SECTION_NAME: &str = "Pinned";

/// Environment variable for overriding the SQLite state database home directory.
pub const SQLITE_HOME_ENV: &str = "CODEX_SQLITE_HOME";
