# Connecting cloud-api to RDS PostgreSQL

Set `DATABASE_CONNECTION_MODE=direct` to bypass Patroni discovery. The default
is `patroni`. Values must be lowercase without surrounding whitespace;
existing deployments and the legacy `postgres-test` test path are
unchanged. In direct mode, database initialization does not require
`POSTGRES_PRIMARY_APP_ID` or `GATEWAY_SUBDOMAIN` (other application features may
still need gateway configuration).

Example application environment:

```sh
DATABASE_CONNECTION_MODE=direct
DATABASE_HOST=cloudapi-postgres-staging.cx0gm2caeqha.us-east-1.rds.amazonaws.com
DATABASE_PORT=5432
DATABASE_NAME=postgres
DATABASE_USERNAME=cloud_api_app
DATABASE_PASSWORD_FILE=/run/secrets/rds-app-password
DATABASE_TLS_ENABLED=true
DATABASE_TLS_CA_CERT_PATH=/run/certs/us-east-1-bundle.pem
DATABASE_MAX_CONNECTIONS=16
```

Create an appropriately privileged RDS application account; the example username
is not provisioned by this PR. Do not use the source replication account for
application writes. The existing password-file handling is reused.

Direct TLS requires encryption and verifies the server certificate chain and
hostname. Supply the official AWS regional CA bundle, mounted read-only inside
the application container. Without an explicit bundle, platform trust is used;
do not assume it includes the RDS CA. Missing/invalid CA files fail closed.
Use the actual RDS endpoint, not an IP or an alias absent from its certificate.
See [AWS TLS guidance](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.SSL.html)
and the [regional CA bundle](https://truststore.pki.rds.amazonaws.com/us-east-1/us-east-1-bundle.pem).
Direct mode rejects `DATABASE_TLS_ENABLED=false`, including for localhost.
Local plaintext tests can continue to use the legacy `postgres-test` path.
The old insecure test/Patroni
TLS behavior is not changed by this patch.

This mode uses one pool for application reads and writes against the configured
writer endpoint. Hostname padding is trimmed; empty database/user names are rejected.
Pool wait, creation and recycling have explicit timeouts of 5, 10 and 5 seconds,
respectively, in addition to the 10-second socket connection timeout. Recycled
connections run a verification query before checkout. These are per-operation
bounds, not an overall request/query deadline, and are not currently configurable.
It does not discover RDS read replicas. On failover, existing
connections can break; new connections resolve the endpoint again. Validate
application retry behavior; this does not guarantee interruption-free failover.

## Deployment and migration gates

- Wire the new environment variable, password secret and trusted CA file into
  the staging deployment in cvm-ansible-playbooks. Review the resulting attested
  configuration through the normal deployment process. This PR does not do that.
- Verify connectivity from the application CVM to RDS, including security groups,
  routes and DNS. The TLS bridge used by RDS to subscribe to the source CVM is
  a different connection path and is not needed for application-to-RDS access.
- Prepare compatible schemas/extensions and coordinate existing startup migrations
  with logical replication. Changing the endpoint does not copy any data.
- Preserve the field-encryption keys, key IDs and settings required to read the
  migrated ciphertext. RDS storage encryption does not replace application keys.
- Rehearse application reads/writes, migrations and RDS failover in staging.
  Coordinate final synchronization, sequence state, write cutover and rollback
  before switching the live application. Keep the application on the source until
  those gates pass; do not let both databases accept independent application writes.

Pools are lazy: building one does not prove network access or authentication.
Live RDS connectivity and migration are not validated merely by unit tests.
