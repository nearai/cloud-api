# Credit provider eligibility

Credit eligibility is evaluated for the serving provider type (`vllm`, `chutes`,
`tinfoil` or `external`), independently of a model's name. A restricted credit type
does not block other eligible credit types belonging to the same organization.
The database preserves credit priority within the eligible types.

One policy version is selected for each logical request. Provider retries and
fallback retain that version, while each attempt checks its own serving provider.
A provider with no eligible funding is skipped without changing provider health.
Other admission failures retain their existing behavior. Native Responses and
Anthropic message requests also supply their serving provider.

Provider eligibility applies in all monetary admission modes. Off and audit modes
record a zero-amount admission without holding funds, so billing can use the same
policy version. Usage keeps the existing budget reservation identifier through
streaming, partial usage and delayed receipts.

The trusted usage admission endpoint accepts `features.serving_provider_type` and
returns `credit_provider_policy_version`. Callers reuse this version for subsequent
attempts of the same request. Admission errors also return it in the
`x-credit-provider-policy-version` response header. An unknown provider can only
use unrestricted credit types.

A database without provider-policy support uses the existing admission contract;
version `0` identifies that compatibility mode. A partially installed policy schema,
unreadable policy or database failure is an error. Once the new contract is present,
requests use versioned provider-aware admission automatically.

Transfers rejected by the database's restricted-credit policy return HTTP 403 with
`restricted_credit_transfer_disabled`. Existing transfer checks and idempotent
replays are preserved.
