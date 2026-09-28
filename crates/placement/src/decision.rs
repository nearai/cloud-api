//! Inputs to a placement decision.
//!
//! `PlaceInput` is minimal for this task (Task 3); Task 6 adds the remaining
//! fields it needs to actually score and pick a replica.

/// Per-request inputs the eligibility rules (`rules.rs`) check a
/// [`crate::snapshot::ReplicaView`] against.
#[derive(Clone, Debug)]
pub struct PlaceInput {
    pub model: String,
    pub prompt_tokens_est: u64,
    /// Host ids known (from Fleet's existing tier knowledge) to serve long
    /// context, passed in by the caller.
    pub long_context_hosts: Vec<String>,
}
