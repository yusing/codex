//! Durable native change-history store for Mekugi.
//! Persists immutable pre-edit baseline bytes and post-edit snapshot bytes per
//! affected file under monotonic stream-scoped Mekugi change IDs (e.g. `amber1`).
//! Shared across `apply_patch` capture, future shell-write capture, and the
//! native `mchanges` review/revert/apply CLI.

mod diff;
mod error;
mod identity;
mod model;
mod ops;
mod store;

pub use diff::ComposedFileDiff;
pub use diff::ComposedSummary;
pub use diff::FileDiffStats;
pub use diff::captured_diff_files;
pub use diff::captured_diff_range;
pub use diff::captured_diff_range_by;
pub use diff::captured_file_stats;
pub use diff::compose_diff;
pub use diff::compose_file_stats;
pub use diff::compose_summary;
pub use diff::render_history;
pub use error::ChangeHistoryError;
pub use identity::ChangeId;
pub use identity::UNATTRIBUTED_STREAM;
pub use identity::expand_id_spec;
pub use identity::stream_name_from_index;
pub use model::ChangeAuthor;
pub use model::ChangeIndexEntry;
pub use model::ChangeMetadata;
pub use model::ChangeRecord;
pub use model::ChangeScope;
pub use model::ChangeSourceKind;
pub use model::CoverageStatus;
pub use model::FileChangeKind;
pub use model::FileEvidence;
pub use model::FileSnapshot;
pub use model::FileState;
pub use model::PendingFileChange;
pub use model::classify_change_scope;
pub use ops::MutationDirection;
pub use ops::MutationOutcome;
pub use store::ChangeHistoryStore;

/// Environment variable pointing child shells and `mchanges` at the active
/// session's native change-history store directory.
pub const MEKUGI_CHANGE_HISTORY_DIR_ENV_VAR: &str = "MEKUGI_CHANGE_HISTORY_DIR";

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
