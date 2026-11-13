use async_trait::async_trait;
use services::attestation::{
    ports::AttestationRepository, AttestationError, ChatSignature, SignatureKind, SignatureOwner,
};
use tokio_postgres::types::Type;

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

/// Insert columns, in placeholder order.
const INSERT_COLUMNS: &str = "INSERT INTO chat_signatures (chat_id, text, signature, signing_address, signing_algo, signature_kind, organization_id, workspace_id) VALUES ";
const COLUMNS_PER_ROW: usize = 8;

/// The unique key is still `(chat_id, signing_algo)` alone, which the previous
/// release's statement names during a rolling deploy. The `WHERE` lets a row
/// be replaced only by the owner it was stored for: a row of another owner,
/// or a row with no owner (NULL never compares equal), is left unchanged and
/// counts as not written. The owner columns are never in the `SET` list, so a
/// write cannot move a row to another owner.
const UPSERT_SUFFIX: &str = " ON CONFLICT (chat_id, signing_algo) DO UPDATE SET text = EXCLUDED.text, signature = EXCLUDED.signature, signing_address = EXCLUDED.signing_address, signature_kind = EXCLUDED.signature_kind, updated_at = NOW() WHERE chat_signatures.organization_id = EXCLUDED.organization_id AND chat_signatures.workspace_id = EXCLUDED.workspace_id";

/// Build the upsert for `rows` signatures of one chat id.
fn upsert_sql(rows: usize) -> String {
    let mut sql = String::from(INSERT_COLUMNS);
    for index in 0..rows {
        if index > 0 {
            sql.push_str(", ");
        }
        let base = index * COLUMNS_PER_ROW;
        let placeholders: Vec<String> = (1..=COLUMNS_PER_ROW)
            .map(|offset| format!("${}", base + offset))
            .collect();
        sql.push('(');
        sql.push_str(&placeholders.join(", "));
        sql.push(')');
    }
    sql.push_str(UPSERT_SUFFIX);
    sql
}

impl PgAttestationRepository {
    /// Insert or replace `signatures` for `owner` in one statement. Fails with
    /// `OwnerConflict` when any row was held by another owner or by no owner;
    /// the rows that were this owner's are written.
    async fn upsert(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signatures: &[ChatSignature],
    ) -> Result<(), AttestationError> {
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
        let sql = upsert_sql(signatures.len());
        let mut params: Vec<(&(dyn tokio_postgres::types::ToSql + Sync), Type)> =
            Vec::with_capacity(signatures.len() * COLUMNS_PER_ROW);
        for (signature, signature_kind) in signatures.iter().zip(signature_kinds.iter()) {
            params.push((&chat_id, Type::VARCHAR));
            params.push((&signature.text, Type::TEXT));
            params.push((&signature.signature, Type::TEXT));
            params.push((&signature.signing_address, Type::VARCHAR));
            params.push((&signature.signing_algo, Type::VARCHAR));
            params.push((signature_kind, Type::VARCHAR));
            params.push((&owner.organization_id, Type::UUID));
            params.push((&owner.workspace_id, Type::UUID));
        }

        // One-shot typed query: one round trip, see repositories/mod.rs.
        let written = client
            .execute_typed(sql.as_str(), &params)
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        if written < signatures.len() as u64 {
            tracing::warn!(
                organization_id = %owner.organization_id,
                workspace_id = %owner.workspace_id,
                requested = signatures.len(),
                written,
                "Chat signature id already stored for another owner; not replaced"
            );
            return Err(AttestationError::OwnerConflict);
        }
        Ok(())
    }
}

#[async_trait]
impl AttestationRepository for PgAttestationRepository {
    async fn add_chat_signature(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signature: ChatSignature,
    ) -> Result<(), AttestationError> {
        self.upsert(owner, chat_id, std::slice::from_ref(&signature))
            .await
    }

    /// One round trip for all signatures of a chat: the streaming path stores
    /// an ecdsa and an ed25519 signature before the client sees `[DONE]`, and
    /// on a replica far from the database each statement is a full network
    /// round trip.
    async fn add_chat_signatures(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signatures: Vec<ChatSignature>,
    ) -> Result<(), AttestationError> {
        let signatures = last_signature_per_algo(signatures);
        self.upsert(owner, chat_id, &signatures).await
    }

    /// Rows with a NULL owner (stored before owners were recorded) never
    /// match, so they are not found for any caller.
    async fn get_chat_signature(
        &self,
        owner: SignatureOwner,
        chat_id: &str,
        signing_algo: &str,
    ) -> Result<ChatSignature, AttestationError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        let row = client
            .query_opt(
                "SELECT text, signature, signing_address, signing_algo, signature_kind FROM chat_signatures WHERE chat_id = $1 AND signing_algo = $2 AND organization_id = $3 AND workspace_id = $4",
                &[
                    &chat_id,
                    &signing_algo,
                    &owner.organization_id,
                    &owner.workspace_id,
                ],
            )
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?
            .ok_or_else(|| {
                AttestationError::SignatureNotFound(format!("{chat_id}:{signing_algo}"))
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

    #[test]
    fn upsert_binds_the_owner_per_row_and_never_moves_it() {
        let sql = upsert_sql(2);
        assert!(sql.contains("($1, $2, $3, $4, $5, $6, $7, $8), "));
        assert!(sql.contains("($9, $10, $11, $12, $13, $14, $15, $16)"));
        assert!(!sql.contains("$17"));
        // The update is guarded by the stored owner, and the owner columns
        // are not assigned.
        let (_, set_clause) = sql.split_once("DO UPDATE SET").unwrap();
        assert!(set_clause.contains(
            "WHERE chat_signatures.organization_id = EXCLUDED.organization_id AND chat_signatures.workspace_id = EXCLUDED.workspace_id"
        ));
        let assignments = set_clause.split(" WHERE ").next().unwrap();
        assert!(!assignments.contains("organization_id"));
        assert!(!assignments.contains("workspace_id"));
        // Rolling compatibility: the arbiter is the key the previous release uses.
        assert!(sql.contains("ON CONFLICT (chat_id, signing_algo)"));
    }
}
