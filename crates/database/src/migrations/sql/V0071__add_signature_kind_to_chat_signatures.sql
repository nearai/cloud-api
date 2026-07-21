-- Record which key produced each stored chat signature:
--   'provider_tee' — signed inside the model-serving TEE over the provider's
--                    canonical text "{model_id}:{request_hash}:{response_hash}"
--   'gateway'      — signed by the cloud-api gateway TEE over
--                    "{request_hash}:{response_hash}" covering the exact bytes
--                    returned to the client (stream rewrites, Chutes fallback)
-- Nullable: rows written before this column existed have unknown provenance
-- (both provider-TEE and gateway writes predate it) and stay NULL; readers
-- surface NULL as "kind absent" rather than guessing.
ALTER TABLE chat_signatures ADD COLUMN signature_kind VARCHAR(50);
