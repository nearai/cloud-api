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
default ID, historical backfill, migration, or new deletion restriction.
Existing deletion rules continue to apply. This addresses the ordering part of
#1088, not its original permanent-designation/deletion-protection requirements.

`GET /v1/organizations` accepts `order_by=joined_at` with either `asc` or `desc`
as `order_direction`. Its existing default remains organization creation order.
The ID tie-breaker is ascending in either direction.

Deploy the API before the companion UI PR. No signup drain is necessary.
