-- MCP Connectors table for storing external MCP server configurations
CREATE TABLE mcp_connectors (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    name VARCHAR(255) NOT NULL,
    description TEXT,
    mcp_server_url TEXT NOT NULL,
    auth_type VARCHAR(50) NOT NULL CHECK (auth_type IN ('none', 'bearer')),
    auth_config JSONB, -- Stores OAuth credentials or API key configuration
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_by UUID NOT NULL REFERENCES users(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_connected_at TIMESTAMPTZ,
    connection_status VARCHAR(50) DEFAULT 'pending', -- pending, connected, failed
    error_message TEXT,
    capabilities JSONB, -- Store discovered MCP server capabilities
    metadata JSONB, -- Additional metadata about the connector
    UNIQUE(organization_id, name)
);

CREATE INDEX idx_mcp_connectors_organization ON mcp_connectors(organization_id);
CREATE INDEX idx_mcp_connectors_active ON mcp_connectors(is_active);
CREATE INDEX idx_mcp_connectors_status ON mcp_connectors(connection_status);

-- MCP Connector Usage Logs for tracking usage and debugging
CREATE TABLE mcp_connector_usage (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    connector_id UUID NOT NULL REFERENCES mcp_connectors(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id),
    method VARCHAR(100) NOT NULL,
    request_payload JSONB,
    response_payload JSONB,
    status_code INTEGER,
    error_message TEXT,
    duration_ms INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_mcp_usage_connector ON mcp_connector_usage(connector_id);
CREATE INDEX idx_mcp_usage_user ON mcp_connector_usage(user_id);
CREATE INDEX idx_mcp_usage_created ON mcp_connector_usage(created_at);
