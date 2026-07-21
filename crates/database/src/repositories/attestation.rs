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
