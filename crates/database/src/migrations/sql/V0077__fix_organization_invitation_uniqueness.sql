ALTER TABLE organization_invitations
DROP CONSTRAINT IF EXISTS organization_invitations_organization_id_email_status_key;

WITH ranked_pending AS (
    SELECT id,
           ROW_NUMBER() OVER (
               PARTITION BY organization_id, LOWER(email)
               ORDER BY created_at DESC, id DESC
           ) AS position
    FROM organization_invitations
    WHERE status = 'pending'
)
UPDATE organization_invitations invitation
SET status = 'expired'
FROM ranked_pending
WHERE invitation.id = ranked_pending.id
  AND ranked_pending.position > 1;

CREATE UNIQUE INDEX unique_pending_organization_invitation
ON organization_invitations (organization_id, LOWER(email))
WHERE status = 'pending';
