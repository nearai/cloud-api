-- Response items table for storing individual response output items
-- This enables granular storage of messages, tool calls, reasoning, etc.
CREATE TABLE response_items (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    response_id UUID NOT NULL REFERENCES responses(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conversation_id UUID REFERENCES conversations(id) ON DELETE CASCADE,
    item JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for efficient querying
CREATE INDEX idx_response_items_response ON response_items(response_id);
CREATE INDEX idx_response_items_user ON response_items(user_id);
CREATE INDEX idx_response_items_conversation ON response_items(conversation_id);
CREATE INDEX idx_response_items_created ON response_items(created_at);

-- Trigger for updated_at
CREATE TRIGGER update_response_items_updated_at 
    BEFORE UPDATE ON response_items
    FOR EACH ROW EXECUTE FUNCTION update_updated_at_column();

