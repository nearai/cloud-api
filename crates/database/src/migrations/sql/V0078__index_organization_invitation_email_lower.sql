CREATE INDEX idx_org_invitations_email_lower
ON organization_invitations (LOWER(email));
