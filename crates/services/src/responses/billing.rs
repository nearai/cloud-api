//! Request-scoped billing correlation for the typed stateless Responses adapter.
//!
//! The completion layer records one usage row per inference round, keyed by
//! the UUID derived from that round's provider completion ID, and only when the
//! round reported final usage. This record mirrors those rounds so the HTTP
//! layer can expose the same identifiers and refuse to report success for a
//! round that could not be billed. Provider IDs stay inside this record; only
//! the derived UUIDs are returned.

use crate::completions::hash_inference_id_to_uuid;
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

#[derive(Debug, Default)]
struct BillingRound {
    /// Last provider completion ID observed in the round. The completion
    /// layer keys its usage row by the last ID it sees, so this does too.
    provider_id: Option<String>,
    usage_reported: bool,
    /// Whether the last reported usage had any tokens. The completion layer
    /// writes no row for a round with zero input and zero output tokens.
    has_tokens: bool,
}

/// Why a round cannot be matched to a usage row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnbilledRound {
    /// The round ended without a usage report.
    MissingUsage,
    /// The round reported usage without a provider completion ID.
    MissingIdentity,
}

/// Shared between the response worker and the HTTP adapter for one request.
#[derive(Clone, Default)]
pub struct ResponseBilling {
    rounds: Arc<Mutex<Vec<BillingRound>>>,
}

impl ResponseBilling {
    fn rounds(&self) -> MutexGuard<'_, Vec<BillingRound>> {
        // The guarded data is plain bookkeeping that stays valid after a panic.
        self.rounds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Update the current round, starting one if none has begun.
    fn update_current(&self, update: impl FnOnce(&mut BillingRound)) {
        let mut rounds = self.rounds();
        match rounds.last_mut() {
            Some(round) => update(round),
            None => {
                let mut round = BillingRound::default();
                update(&mut round);
                rounds.push(round);
            }
        }
    }

    /// Start tracking a new inference round.
    pub fn begin_round(&self) {
        self.rounds().push(BillingRound::default());
    }

    /// Record the provider completion ID carried by a streamed chunk.
    pub fn record_provider_id(&self, provider_id: &str) {
        if provider_id.is_empty() {
            return;
        }
        self.update_current(|round| {
            if round.provider_id.as_deref() != Some(provider_id) {
                round.provider_id = Some(provider_id.to_owned());
            }
        });
    }

    /// Record a usage report for the current round. Reports are cumulative
    /// within a round, so the latest one decides whether it has tokens.
    pub fn record_usage(&self, input_tokens: i32, output_tokens: i32) {
        self.update_current(|round| {
            round.usage_reported = true;
            round.has_tokens = input_tokens != 0 || output_tokens != 0;
        });
    }

    /// Identifier of the first round, available once its first chunk arrives.
    pub fn first_inference_id(&self) -> Option<Uuid> {
        self.rounds()
            .first()
            .and_then(|round| round.provider_id.as_deref())
            .map(hash_inference_id_to_uuid)
    }

    /// Identifiers of every round with a usage row, in round order. Fails when
    /// any round ran without the usage report or completion ID the completion
    /// layer needs to record it.
    pub fn billed_inference_ids(&self) -> Result<Vec<Uuid>, UnbilledRound> {
        let rounds = self.rounds();
        let mut ids = Vec::with_capacity(rounds.len());
        for round in rounds.iter() {
            if !round.usage_reported {
                return Err(UnbilledRound::MissingUsage);
            }
            let Some(provider_id) = round.provider_id.as_deref() else {
                return Err(UnbilledRound::MissingIdentity);
            };
            if !round.has_tokens {
                return Err(UnbilledRound::MissingUsage);
            }
            let id = hash_inference_id_to_uuid(provider_id);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Err(UnbilledRound::MissingUsage);
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_map_to_the_completion_layer_identifiers_in_order() {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_provider_id("chatcmpl-search");
        billing.record_usage(12, 4);
        billing.begin_round();
        billing.record_provider_id("chatcmpl-answer");
        billing.record_usage(40, 9);

        assert_eq!(
            billing.first_inference_id(),
            Some(hash_inference_id_to_uuid("chatcmpl-search"))
        );
        assert_eq!(
            billing.billed_inference_ids(),
            Ok(vec![
                hash_inference_id_to_uuid("chatcmpl-search"),
                hash_inference_id_to_uuid("chatcmpl-answer"),
            ])
        );
    }

    #[test]
    fn last_provider_id_in_a_round_wins() {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_provider_id("chatcmpl-a");
        billing.record_provider_id("");
        billing.record_provider_id("chatcmpl-b");
        billing.record_usage(1, 1);
        assert_eq!(
            billing.billed_inference_ids(),
            Ok(vec![hash_inference_id_to_uuid("chatcmpl-b")])
        );
    }

    #[test]
    fn a_round_without_final_usage_is_unbilled() {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_provider_id("chatcmpl-1");
        billing.record_usage(5, 5);
        billing.begin_round();
        billing.record_provider_id("chatcmpl-2");
        assert_eq!(
            billing.billed_inference_ids(),
            Err(UnbilledRound::MissingUsage)
        );
    }

    #[test]
    fn usage_without_a_completion_id_is_unbilled() {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_usage(5, 5);
        assert_eq!(
            billing.billed_inference_ids(),
            Err(UnbilledRound::MissingIdentity)
        );
        assert_eq!(billing.first_inference_id(), None);
    }

    #[test]
    fn zero_token_rounds_cannot_confirm_a_usage_row() {
        let billing = ResponseBilling::default();
        billing.begin_round();
        billing.record_provider_id("chatcmpl-empty");
        billing.record_usage(0, 0);
        assert_eq!(
            billing.billed_inference_ids(),
            Err(UnbilledRound::MissingUsage)
        );
    }

    #[test]
    fn no_rounds_cannot_confirm_usage() {
        assert_eq!(
            ResponseBilling::default().billed_inference_ids(),
            Err(UnbilledRound::MissingUsage)
        );
    }
}
