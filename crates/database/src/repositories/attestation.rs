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
            .execute(
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
