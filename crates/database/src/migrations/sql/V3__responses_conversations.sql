-- Conversations table for storing conversation metadata
CREATE TABLE conversations (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    metadata JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_conversations_user ON conversations(user_id);
CREATE INDEX idx_conversations_created ON conversations(created_at);
CREATE INDEX idx_conversations_updated ON conversations(updated_at);

-- Responses table for storing AI response data
CREATE TABLE responses (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    model VARCHAR(255) NOT NULL,
    input_messages JSONB NOT NULL,
    output_message TEXT,
    status VARCHAR(20) NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed', 'cancelled')),
    instructions TEXT,
    conversation_id UUID REFERENCES conversations(id) ON DELETE CASCADE,
    previous_response_id UUID REFERENCES responses(id) ON DELETE SET NULL,
    usage JSONB,
    metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_responses_user ON responses(user_id);
CREATE INDEX idx_responses_status ON responses(status);
CREATE INDEX idx_responses_conversation ON responses(conversation_id);
CREATE INDEX idx_responses_previous ON responses(previous_response_id);
CREATE INDEX idx_responses_created ON responses(created_at);