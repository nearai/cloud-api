# Default organization by membership order

`GET /v1/users/me` returns active organizations ordered by the requesting user's
membership `joined_at ASC`, then organization ID `ASC` to break timestamp ties.
Sorting happens in PostgreSQL before the existing 100-item limit, so the earliest
membership is included even when the user has more than 100 organizations.

The first active membership is the default. Staking validation and VPC login
explicitly request the same order. UI consumers preserve this response order;
organization `created_at` does not determine the default. A saved dashboard
selection can still select a different active organization without changing the
default used by managed Playground or staking.

This is a dynamic default: removing the first membership or deactivating its
organization advances the default to the next available membership. Rejoining
an organization uses the new membership's timestamp. There is no persisted
default ID, historical backfill, or migration.

The organization deletion API rejects an organization that is the earliest
active membership of any current member, even when it is not the requesting
owner's default. It returns HTTP 409 with error type `default_organization`.
The repository checks the full membership set inside the existing deletion
transaction, after ownership and staking restrictions, while holding the target
organization row lock. Non-default organizations remain deletable under the
existing rules. The UI hides deletion for the signed-in user's default and
handles server rejections for other members' defaults.

Adding a member, including accepting an invitation, checks that the organization
is active and holds a `FOR SHARE` row lock through the membership insert in the
same transaction. This serializes with deletion's `FOR UPDATE` lock: deletion
sees a committed new member, or the insert rejects the deleted organization.
Accepting an outstanding invitation to a deleted organization returns HTTP 404
without creating a membership or marking the invitation accepted.

Protection follows the dynamic definition: if a member leaves, their former
organization is no longer protected on their behalf. Ownership transfer alone
does not remove a retained membership's protection. This does not restore the
original permanent-designation requirement in #1088 or prevent administrative
SQL repairs/deactivation outside the user-facing deletion API.

`GET /v1/organizations` accepts `order_by=joined_at` with either `asc` or `desc`
as `order_direction`. Its existing default remains organization creation order.
The ID tie-breaker is ascending in either direction.

Deploy the API before the companion UI PR. No signup drain is necessary.
