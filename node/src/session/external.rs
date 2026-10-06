//! A harness the operator runs themselves, calling a channel's tools through
//! the operator door.
//!
//! Such a harness has no session: every call stands alone, is logged to the
//! channel's external log under the lane the harness gives, and is fenced by
//! the channel's Stop. Before that, each client attached as a session row;
//! those rows remain, closed, and are recognised by [`HARNESS_ID`].

/// The `harness_id` an attached session recorded. Not an adapter: nothing is
/// launched, and `adapter_for` never sees it. The interface keys its wording
/// off this for old rows, and `recent_repos` filters on it.
pub const HARNESS_ID: &str = "external";
