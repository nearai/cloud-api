use crate::repositories::statement_cache::CachedStatements;
use async_trait::async_trait;
use services::attestation::{
    ports::AttestationRepository, AttestationError, ChatSignature, SignatureKind,
};

use crate::DbPool;

pub struct PgAttestationRepository {
    pool: DbPool,
}

impl PgAttestationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    fn row_to_chat_signature(
        &self,
        row: tokio_postgres::Row,
    ) -> Result<ChatSignature, AttestationError> {
        let text: String = row
            .try_get("text")
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        let signature: String = row
            .try_get("signature")
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        let signing_address: String = row
            .try_get("signing_address")
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        let signing_algo: String = row
            .try_get("signing_algo")
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        // NULL (legacy rows) and unrecognized values both surface as `None`:
        // the kind is unknown, not guessed.
        let signature_kind: Option<String> = row
            .try_get("signature_kind")
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        let signature_kind = signature_kind
            .as_deref()
            .and_then(SignatureKind::from_db_str);
        Ok(ChatSignature {
            text,
            signature,
            signing_address,
            signing_algo,
            signature_kind,
        })
    }
}

/// Collapse signatures that share a `signing_algo`, keeping the last one.
///
/// A multi-row `INSERT ... ON CONFLICT DO UPDATE` raises SQLSTATE 21000
/// ("cannot affect row a second time") when two rows of the same statement
/// hit the same arbiter key, whereas writing them one at a time simply let
/// the later row overwrite the earlier. `signing_algo` is supplied by the
/// inference backend, so the batch must tolerate duplicates the same way.
fn last_signature_per_algo(signatures: Vec<ChatSignature>) -> Vec<ChatSignature> {
    let mut deduped: Vec<ChatSignature> = Vec::with_capacity(signatures.len());
    for signature in signatures {
        match deduped
            .iter_mut()
            .find(|existing| existing.signing_algo == signature.signing_algo)
        {
            Some(existing) => *existing = signature,
            None => deduped.push(signature),
        }
    }
    deduped
}

#[async_trait]
impl AttestationRepository for PgAttestationRepository {
    async fn add_chat_signature(
        &self,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        let signature_kind = signature.signature_kind.map(|kind| kind.as_str());
        client
            .cached_execute(
                "INSERT INTO chat_signatures (chat_id, text, signature, signing_address, signing_algo, signature_kind) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (chat_id, signing_algo) DO UPDATE SET text = EXCLUDED.text, signature = EXCLUDED.signature, signing_address = EXCLUDED.signing_address, signature_kind = EXCLUDED.signature_kind, updated_at = NOW()",
                &[&chat_id, &signature.text, &signature.signature, &signature.signing_address, &signature.signing_algo, &signature_kind],
            )
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        Ok(())
    }

    /// One round trip for all signatures of a chat: the streaming path stores
    /// an ecdsa and an ed25519 signature before the client sees `[DONE]`, and
    /// on a replica far from the database each statement is a full network
    /// round trip.
    async fn add_chat_signatures(
        &self,
        chat_id: &str,
        signatures: Vec<ChatSignature>,
    ) -> Result<(), AttestationError> {
        let signatures = last_signature_per_algo(signatures);
        if signatures.is_empty() {
            return Ok(());
        }
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        let signature_kinds: Vec<Option<&str>> = signatures
            .iter()
            .map(|signature| signature.signature_kind.map(|kind| kind.as_str()))
            .collect();
        let mut sql = String::from(
            "INSERT INTO chat_signatures (chat_id, text, signature, signing_address, signing_algo, signature_kind) VALUES ",
        );
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
        for (index, (signature, signature_kind)) in
            signatures.iter().zip(signature_kinds.iter()).enumerate()
        {
            if index > 0 {
                sql.push_str(", ");
            }
            let base = index * 6;
            sql.push_str(&format!(
                "(${}, ${}, ${}, ${}, ${}, ${})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6
            ));
            params.push(&chat_id);
            params.push(&signature.text);
            params.push(&signature.signature);
            params.push(&signature.signing_address);
            params.push(&signature.signing_algo);
            params.push(signature_kind);
        }
        sql.push_str(
            " ON CONFLICT (chat_id, signing_algo) DO UPDATE SET text = EXCLUDED.text, signature = EXCLUDED.signature, signing_address = EXCLUDED.signing_address, signature_kind = EXCLUDED.signature_kind, updated_at = NOW()",
        );

        client
            .execute(sql.as_str(), &params)
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        Ok(())
    }

    async fn get_chat_signature(
        &self,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        let row = client
            .query_one(
                "SELECT * FROM chat_signatures WHERE chat_id = $1 AND signing_algo = $2",
                &[&chat_id, &signing_algo],
            )
            .await
            .map_err(|e| {
                // query_one returns RowNotFound when no rows are found
                if e.to_string()
                    .contains("query returned an unexpected number of rows")
                {
                    return AttestationError::SignatureNotFound(format!(
                        "{chat_id}:{signing_algo}"
                    ));
                }
                AttestationError::RepositoryError(e.to_string())
            })?;
        self.row_to_chat_signature(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signature(algo: &str, text: &str) -> ChatSignature {
        ChatSignature {
            text: text.to_string(),
            signature: format!("sig-{text}"),
            signing_address: "addr".to_string(),
            signing_algo: algo.to_string(),
            signature_kind: None,
        }
    }

    #[test]
    fn distinct_algorithms_are_kept_in_order() {
        let out = last_signature_per_algo(vec![signature("ecdsa", "a"), signature("ed25519", "b")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].signing_algo, "ecdsa");
        assert_eq!(out[1].signing_algo, "ed25519");
    }

    #[test]
    fn duplicate_algorithm_keeps_the_last_signature() {
        let out = last_signature_per_algo(vec![
            signature("ecdsa", "first"),
            signature("ed25519", "b"),
            signature("ecdsa", "second"),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].signing_algo, "ecdsa");
        assert_eq!(out[0].text, "second");
        assert_eq!(out[1].signing_algo, "ed25519");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert!(last_signature_per_algo(Vec::new()).is_empty());
    }
}
