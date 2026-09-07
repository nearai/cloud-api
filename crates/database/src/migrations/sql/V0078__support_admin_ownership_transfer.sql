ALTER TABLE organization_member_role_audit_log
DROP CONSTRAINT organization_member_role_audit_log_previous_role_check;

ALTER TABLE organization_member_role_audit_log
ADD CONSTRAINT organization_member_role_audit_log_previous_role_check
CHECK (previous_role IN ('owner', 'admin', 'member'));

ALTER TABLE organization_member_role_audit_log
DROP CONSTRAINT organization_member_role_audit_log_new_role_check;

ALTER TABLE organization_member_role_audit_log
ADD CONSTRAINT organization_member_role_audit_log_new_role_check
CHECK (new_role IN ('owner', 'admin', 'member'));

-- Earlier invitation flows could create more than one owner. Keep the original
-- owner and convert additional owner memberships before enforcing uniqueness.
WITH ranked_owners AS (
    SELECT
        id,
        ROW_NUMBER() OVER (
            PARTITION BY organization_id
            ORDER BY joined_at, id
        ) AS owner_position
    FROM organization_members
    WHERE role = 'owner'
)
UPDATE organization_members AS member
SET role = 'admin'
FROM ranked_owners
WHERE member.id = ranked_owners.id
  AND ranked_owners.owner_position > 1;

CREATE UNIQUE INDEX unique_organization_owner
ON organization_members (organization_id)
WHERE role = 'owner';
