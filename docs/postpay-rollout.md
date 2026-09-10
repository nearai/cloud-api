# Postpay rollout

Postpay must be deployed API-first. Complete these steps in order:

1. Deploy the Cloud API version that reads and writes `credit_type = 'postpay'`.
2. Have billing/operations approve an explicit list of legacy contract organization UUIDs, their current grant amounts, and their postpay ceilings.
3. Convert only those organizations with [`scripts/convert-contract-grants-to-postpay.sql`](../scripts/convert-contract-grants-to-postpay.sql). First run a copy ending in `ROLLBACK`, review its verification output, then run the approved copy ending in `COMMIT`.
4. Confirm each converted organization has no active legacy contract grant, exactly one positive active postpay row, the expected total active limit, and is classified as paying in admin analytics.
5. Deploy the admin UI and customer UI changes that create and display postpay limits.

Do not bulk-convert every active grant. A free promotional grant and a postpay contract ceiling may legitimately coexist. The conversion list is required because the historical data does not contain enough information to distinguish those grants from contract ceilings safely.

Setting an active postpay row to zero disables postpay. Zero postpay rows remain in the audit history and balance breakdown but are not classified as paying in platform or organization analytics.
