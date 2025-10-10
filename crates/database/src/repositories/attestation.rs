use async_trait::async_trait;
use services::attestation::{ports::AttestationRepository, AttestationError, ChatSignature};

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
        Ok(ChatSignature {
            text,
            signature,
            signing_address,
            signing_algo,
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
        client
            .execute(
                "INSERT INTO chat_signatures (chat_id, text, signature, signing_address, signing_algo) VALUES ($1, $2, $3, $4, $5)",
                &[&chat_id, &signature.text, &signature.signature, &signature.signing_address, &signature.signing_algo],
            )
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;

        Ok(())
    }

    async fn get_chat_signature(&self, chat_id: &str) -> Result<ChatSignature, AttestationError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| AttestationError::RepositoryError(e.to_string()))?;
        let row = client
            .query_one(
                "SELECT * FROM chat_signatures WHERE chat_id = $1",
                &[&chat_id],
            )
            .await
            .map_err(|e| {
                // query_one returns RowNotFound when no rows are found
                if e.to_string()
                    .contains("query returned an unexpected number of rows")
                {
                    return AttestationError::SignatureNotFound(chat_id.to_string());
                }
                AttestationError::RepositoryError(e.to_string())
            })?;
        self.row_to_chat_signature(row)
    }
}
