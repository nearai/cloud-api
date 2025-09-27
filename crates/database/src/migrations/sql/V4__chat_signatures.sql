-- Chat signatures table for cryptographic verification of chat content
CREATE TABLE chat_signatures (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    chat_id VARCHAR(255) NOT NULL UNIQUE,
    text TEXT NOT NULL,
    signature TEXT NOT NULL,
    signing_address VARCHAR(255) NOT NULL,
    signing_algo VARCHAR(50) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_chat_signatures_chat_id ON chat_signatures(chat_id);
CREATE INDEX idx_chat_signatures_signing_address ON chat_signatures(signing_address);
CREATE INDEX idx_chat_signatures_signing_algo ON chat_signatures(signing_algo);
CREATE INDEX idx_chat_signatures_created ON chat_signatures(created_at);
