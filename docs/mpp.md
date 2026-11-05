# Machine Payments Protocol (MPP)

MPP lets an agent with a Tempo wallet and no NEAR AI API key call
`POST /v1/chat/completions`, pay per request in a TIP-20 stablecoin, and get the
completion plus a `Payment-Receipt`. It is the IETF `Payment` HTTP authentication
scheme ([draft-httpauth-payment](https://paymentauth.org), [mpp.dev](https://mpp.dev)),
method `tempo`, intent `charge`.

MPP is **off by default**. With `MPP_ENABLED` unset or false nothing else is read or
validated, and every request behaves exactly as before.

## Flow

```
client                                         cloud-api                        Tempo
  | POST /v1/chat/completions (no Authorization) |                                 |
  |--------------------------------------------->|                                 |
  | 402 + WWW-Authenticate: Payment id=…, …      |                                 |
  |<---------------------------------------------|                                 |
  |  transferWithMemo(recipient, amount, memo)   |                                 |
  |------------------------------------------------------------------------------->|
  | POST … Authorization: Payment <credential>   |                                 |
  |--------------------------------------------->| eth_sendRawTransactionSync /    |
  |                                              | eth_getTransactionReceipt       |
  |                                              |-------------------------------->|
  | 200 + Payment-Receipt (no-store)             |                                 |
  |<---------------------------------------------|                                 |
```

1. A request with no `Authorization` header for a **priced** model, whose body passes the
   handler's own checks (JSON content type, request shape, `validate_request`), gets
   `402 Payment Required` with `WWW-Authenticate: Payment id, realm, method="tempo",
   intent="charge", request, expires, digest, opaque`, `Cache-Control: no-store`, and an
   `application/problem+json` body (`https://paymentauth.org/problems/payment-required`).
2. The client transfers the requested amount of the configured TIP-20 token to the
   configured recipient with `transferWithMemo`. The memo is the MPP attribution memo
   bound to this server's realm and this challenge id (mppx does this by default).
3. The client retries the **same body** with `Authorization: Payment <base64url(JSON)>`
   carrying a `transaction` payload (a signed `0x76` Tempo transaction the server checks and
   then broadcasts with `eth_sendRawTransactionSync`) or a `hash` payload (a transaction the
   client already sent). An optional `source` is a `did:pkh:eip155:<chainId>:0x<address>`.
4. The server verifies the credential and the payment on chain, and only then runs the
   request as the configured service key (an unpaid credential never touches that key's rate
   limit). The payment is claimed once, just before the handler, and a successful response
   carries a `Payment-Receipt` (`base64url` of
   `{"method":"tempo","reference":"0x<tx hash>","status":"success","timestamp":…}`). An error
   response carries no receipt.

Streaming responses are supported: the receipt headers are on the response head. The MPP
layer marks the response `Cache-Control: private`; the app-wide request-correlation layer
then sets `no-store` on every response, which is stricter, so a receipt is never cached.

### Status codes

| Status | When |
|---|---|
| `402` + fresh challenge | no credential; a malformed credential; an invalid or expired challenge; a reverted transaction or no matching transfer; a `transaction` payload that does not pay this challenge (refused before any broadcast) |
| `403`, no challenge | the payment was already used, or funds moved without a memo bound to this challenge. Paying again would not help |
| `400` | a priced model whose request breaks a limit (below), or a body the handler would refuse anyway (its own error, no challenge) |
| `400` `method-unsupported` | a `proof` payload, or a method other than `tempo` |
| `503` + `Retry-After`, no challenge | the transaction is not found or not yet mined, or a broadcast could not be confirmed: **retry the same credential**, do not pay again. Also when the Tempo RPC or the claim store did not answer, or the service workspace cannot pay for inference |
| `401` | unpriced or unparseable request without a valid key: today's behavior |

A `Bearer` key request is never touched by MPP, and a request without `Authorization` for
a model that is not priced still gets today's `401`.

## Pricing

Pricing is a **flat price per request per model** (`MPP_MODEL_PRICES`), with caps that keep
the price honest:

- `max_tokens` or `max_completion_tokens` must be present and positive; the larger of the
  two must be at most the model's `max_tokens`.
- `n` must be absent or `1`.
- The request body must be at most the model's `max_request_bytes`.

A priced request that breaks a limit gets `400` naming the limit and no challenge. Keys of
`MPP_MODEL_PRICES` match the `model` string the client sends, exactly. An alias is priced
only if it is listed itself, so list each name you want payable, and give an alias the price
of the model it resolves to.

Metered per-token billing is the MPP `session` intent; see [Sessions](#sessions) below. A
model may advertise both `charge` and `session` (two `WWW-Authenticate` headers).

## Configuration

All variables are read only when `MPP_ENABLED=true`. Invalid configuration is a startup
error. Secrets also accept a `*_FILE` variant (the file wins), and are redacted in `Debug`.

| Variable | Meaning |
|---|---|
| `MPP_ENABLED` | `true` to enable (default `false`) |
| `MPP_REALM` | ASCII host, e.g. `cloud-api.near.ai`. The challenge realm and the attribution server id |
| `MPP_SECRET_KEY` / `_FILE` | HMAC key, at least 32 bytes, identical on every instance |
| `MPP_SERVICE_API_KEY` / `_FILE` | `sk-` key of the internal workspace MPP traffic runs and is metered under |
| `MPP_CHALLENGE_TTL_SECS` | challenge lifetime, 60 to 3600 (default 600) |
| `MPP_TEMPO_RPC_URL` | Tempo JSON-RPC. `https`, or `http` on a loopback host for tests |
| `MPP_TEMPO_CHAIN_ID` | chain id (Moderato testnet: `42431`) |
| `MPP_TEMPO_CURRENCY` | the accepted TIP-20 token, `0x` + 40 hex |
| `MPP_TEMPO_RECIPIENT` | treasury address, `0x` + 40 hex |
| `MPP_RPC_TIMEOUT_SECS` | per-call RPC timeout, 1 to 120 (default 15) |
| `MPP_MODEL_PRICES` | JSON, see below |

```json
{"zai-org/GLM-5.1-FP8": {"amount": "20000", "max_tokens": 2048, "max_request_bytes": 65536}}
```

`amount` is a decimal string of token base units, greater than zero and at most `u128`.
TIP-20 stablecoins have 6 decimals, so `"20000"` is 0.02. The parser refuses unknown
fields, a duplicate model name, an empty table, and zero limits.

## Operator setup

1. **Service workspace.** Create an organization and workspace for MPP traffic and an API key
   in it. Its credits and limits bound all MPP traffic: when they run out, paying clients get
   `503` with `Retry-After: 30` instead of a payment request (a payment is never taken
   for a request that cannot be served). All MPP requests share that key's rate limit and
   credits, so one payer's burst can make the others wait (a request refused there leaves its
   payment unclaimed and redeemable). Alert on that key's rate-limit rejections and on its
   organization's remaining credits; usage records attribute all MPP traffic to it, not to
   individual payers.
2. **Recipient treasury.** Use an address you control and monitor. The server never signs or
   sends anything; it only reads Tempo.
3. **Schema.** The single-use ledger is the table `mpp_payment_claims`
   (`V0098__mpp_payment_claims.sql` in `nearai/cloud-platform`). cloud-api does not migrate:
   deploy the cloud-platform migration, then the pin in `scripts/cloud-platform-schema.env`,
   before enabling MPP. The table holds `payment_key` (`tempo:` + hex of
   `HMAC-SHA256(MPP_SECRET_KEY, tx hash)`, never the hash itself), `challenge_id`, `state`
   (`serving`, `served`, `unserved`) and two timestamps. It has no organization, workspace,
   request or payer column. cloud-api purges rows older than 24 hours every hour (a claim
   outlives its challenge, at most one hour, many times over).
4. **Reconciliation.** Payments are on-chain transfers to the recipient carrying an MPP
   attribution memo (`keccak256("mpp")[0..4] ‖ 0x01 ‖ keccak256(realm)[0..10] ‖ client ‖
   keccak256(challenge id)[0..7]`). Reconcile the treasury's `TransferWithMemo` events with
   that tag against inference usage of the service key. A row stuck in `serving` means the
   process died mid-request; the payer was charged and not served.

## Verification

- The challenge id is `HMAC-SHA256(secret, realm|method|intent|request|expires|digest|opaque)`,
  verified in constant time, so a challenge needs no storage. The digest binds the exact
  request body; the opaque value binds the route and a random per-challenge nonce, so two
  identical requests never share a challenge (and a payment cannot be claimed under someone
  else's copy of it). Any change is `invalid-challenge`.
- A payment is accepted only if the receipt has `status == 0x1` and a TIP-20 transfer from the
  configured token to the configured recipient for **exactly** the requested amount, sent by
  the expected sender (the `source` address when the client sends one, else the transaction
  sender), carrying an MPP memo bound to this realm and this challenge id.
- The payment key is a keyed digest of the transaction hash. Claiming is one atomic statement, so concurrent
  presentations of one credential serve it once. If the paid request ends in an error
  response, the claim is released to `unserved` and the same credential (same challenge) can
  be presented again until the challenge expires, without a second payment. A client that
  disconnects while its request runs has used the payment: the claim is marked `served`.
- A `transaction` payload is decoded before it is broadcast: its chain id must be the
  configured one, and one of its calls must be `transferWithMemo` on the configured token, to
  the recipient, for the amount, with a memo bound to this challenge. Anything else is refused
  without reaching the node, so the server neither relays arbitrary transactions nor lets a
  client lose funds to a transaction that cannot pay.
- **Expiry bounds when a client must pay, not when a payment can be redeemed.** A request
  refused after the payment was made (rate limit, the service workspace's credits, an outage)
  must not strand that payment when its challenge runs out. For 12 hours after expiry
  (`LATE_REDEMPTION`) the same credential still verifies; the server then only looks the
  payment up (it never broadcasts after expiry) and accepts it only if its block was produced
  at or before the challenge's expiry. A payment mined later, or a credential presented after
  the window, is `payment-expired`. The window plus the longest challenge stays inside the
  24-hour claim retention (checked at compile time), so a purged claim can never be spent
  again.
- Each claim attempt carries a lease (the row's `updated_at`); `finish` only moves the row
  still holding it, so a retried or late `finish` of an earlier attempt cannot release a newer
  attempt's claim.
- If `eth_sendRawTransactionSync` fails, the receipt is looked up by `keccak256(raw tx)` a
  few times (the node may already have the transaction), all under one deadline. No receipt
  is a `503` to retry the same credential.

## Privacy

The service runs in a TEE and MPP adds no customer data to its storage or logs.

- Never logged, in any field or metric label: credentials, `Authorization` values,
  transaction hashes, payer addresses, `source` DIDs, request bodies, the service API key,
  the HMAC secret and the RPC URL. Logs carry closed outcome codes (for example
  `rpc_unavailable`, `memo_not_bound`).
- The claim table stores a keyed digest of the transaction hash (not linkable to the payer
  without `MPP_SECRET_KEY`) and a challenge id only, for 24 hours.
- Problem details use fixed sentences; no request content is echoed.
- Requests run as the service workspace, so usage is recorded against that workspace and not
  against the payer. There is no link between a payer address and a conversation.

## Limits

- One currency (`MPP_TEMPO_CURRENCY`) on one chain, one recipient.
- No fee sponsorship (`feePayer`), no splits, no machine token, no zero-amount `proof`
  credentials. The `session` intent has its own limits in [Sessions](#sessions).
- `POST /v1/chat/completions` only.
- Retry window for a paid request that failed or was refused: the challenge lifetime plus
  12 hours, for a payment mined before the challenge expired.
- Transaction payloads are decoded and checked (above) but not simulated, and the signer is
  not recovered before broadcast; the receipt's sender is checked afterwards.
- A `hash` payload costs one receipt lookup before any rate limit applies: protect a metered
  RPC with edge rate limiting.
- The `Authorization` header is capped at 16 KiB.

## Client example

```ts
import { Mppx, tempo } from 'mppx/client'
import { privateKeyToAccount } from 'viem/accounts'

const account = privateKeyToAccount(process.env.TEMPO_PRIVATE_KEY as `0x${string}`)
Mppx.create({ methods: [tempo.charge({ account })] }) // patches global fetch

const response = await fetch('https://cloud-api.near.ai/v1/chat/completions', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({
    model: 'zai-org/GLM-5.1-FP8',
    max_tokens: 256,
    messages: [{ role: 'user', content: 'Hello' }],
  }),
})
console.log(response.headers.get('payment-receipt'), await response.json())
```

From a shell, with an `mppx` account funded on the same network:
`npx mppx account create`, then
`npx mppx --network testnet https://cloud-api.near.ai/v1/chat/completions -J '{"model":"zai-org/GLM-5.1-FP8","max_tokens":256,"messages":[{"role":"user","content":"Hi"}]}'`.

## Sessions

The `session` intent pays for inference through a TIP-1034 payment channel on Tempo: one
on-chain deposit into the channel reserve precompile
`0x4D50500000000000000000000000000000000000`, one signed voucher per request, server-side
metering by tokens, periodic on-chain settlement, and cooperative close with refund. Unlike
`charge`, nothing is transferred per request; the payer escrows funds once, authorizes spend
with EIP-712 vouchers, and gets the remainder back on close. Errors and disconnects release
the reservation without charging delivered work; a successful `charge` payment is already on
chain before inference runs.

A priced model with `token_price` in `MPP_MODEL_PRICES` is offered `intent="session"` alongside
or instead of `charge`. The session challenge's `recipient` is the server's settlement signer
(`MPP_TEMPO_SIGNER_KEY`), not `MPP_TEMPO_RECIPIENT`; only that payee can settle and close.

### Flow

```
client                                         cloud-api                        Tempo
  | POST /v1/chat/completions (no Authorization) |                                 |
  |--------------------------------------------->|                                 |
  | 402 + WWW-Authenticate: Payment … session    |                                 |
  |<---------------------------------------------|                                 |
  | open / topUp (signed 0x76 tx to escrow)      |                                 |
  |--------------------------------------------->| eth_sendRawTransactionSync      |
  |                                              |-------------------------------->|
  | 200 ok + Payment-Receipt (management)        |                                 |
  |<---------------------------------------------|                                 |
  | POST … Authorization: Payment voucher        |                                 |
  |--------------------------------------------->| reserve → inference → charge    |
  | 200 + Payment-Receipt (reference=channel)    | settle / close (background)     |
  |<---------------------------------------------|-------------------------------->|
```

1. A request with no `Authorization` for a model with `token_price`, whose body passes the
   same handler checks as `charge`, gets `402 Payment Required` with `WWW-Authenticate: Payment
   id, realm, method="tempo", intent="session", request, expires, opaque` (no `digest`: the
   challenge is bound to the route and a nonce only, and may be reused across requests).
   `request` is base64url(JCS):
   `{"amount":"<price per token>","currency":"0x<token>","recipient":"0x<settlement signer>",
   "suggestedDeposit":"<base units>","unitType":"llm_token","methodDetails":{…}}`.
   `methodDetails` includes `chainId`, `escrowContract":"0x4D50500000000000000000000000000000000000`,
   `minVoucherDelta`, `sessionProtocol":"v2"`, and optionally `sessionSnapshot` when the client's
   voucher is too low (see below).
2. The client opens a channel (`action":"open"`) or tops it up (`action":"topUp"`) with a signed
   Tempo transaction calling the escrow precompile. The server answers `200 {"status":"ok"}` with
   a `Payment-Receipt` and does not run inference.
3. For each completion the client retries with `Authorization: Payment` carrying
   `{"action":"voucher",…}` (same challenge echo, channel id, descriptor, `cumulativeAmount`,
   voucher signature). The server reserves spend, runs the request as the service key, charges
   actual usage when the response completes, and returns a receipt. Cooperative close
   (`action":"close"`) is another management action: `200 ok` and a receipt, no inference.
4. When the highest voucher no longer covers the next request's cost cap, the server responds
   with `402` and a fresh session challenge whose `methodDetails.sessionSnapshot` includes
   `requiredCumulative`; the client signs a voucher for that amount (or tops up if it exceeds
   `deposit`) and retries. Nothing is charged on that refusal.

Management actions (`open`, `topUp`, `close`) always answer `200 {"status":"ok"}` with a
`Payment-Receipt` and never touch inference. Only `voucher` runs the chat handler.

Receipt (`Payment-Receipt`, base64url JSON) for session:
`{"method":"tempo","intent":"session","reference":"0x<channelId>","status":"success",
"timestamp":…,"acceptedCumulative":"…","spent":"…","units":<u64>,…}`. The reference is the
channel id, not a transaction hash. `txHash` is present on `open`, `topUp`, and `close` only.
On a streaming completion the receipt headers are on the response head with the reservation
preview: `spent` includes the request's cost cap, and `units` still counts earlier completed
requests. The final charge happens when the stream ends, so that head receipt is provisional.
It is not replaced on the wire; a subsequent receipt reflects the updated ledger.

### Pricing and metering

Add an optional `"token_price": "<base units per token>"` to each model entry (integer string,
greater than zero). The session challenge sets `amount` to that price and `unitType` to
`llm_token`. The same caps as `charge` apply (`max_tokens`, `n`, `max_request_bytes`).

For each voucher request on a channel:

1. `cost_cap = token_price × (prompt_estimate + max_tokens)` where
   `prompt_estimate = ceil(body_bytes / 4)` (an approximation; billing uses real usage after
   the response). Use the larger of the request's `max_tokens` and `max_completion_tokens`
   when both are present. Arithmetic overflow returns `400` before a reservation.
2. Require `accepted_cumulative - spent >= cost_cap`. Otherwise return `402` with
   `sessionSnapshot.requiredCumulative = spent + cost_cap` and run no inference (nothing
   charged).
3. Reserve `cost_cap` before inference (`spent` and `reserved` increase; a reservation row
   keyed by request id) so concurrent requests cannot spend the same balance. After the
   response: `actual = token_price × (prompt_tokens + completion_tokens)` from completion
   usage, capped at what the voucher still covers; then adjust `spent` and `units`, and delete
   the reservation. On an error response or client disconnect, release the full reservation
   (`spent -= cost_cap`): nothing delivered, nothing charged.

Reservations precede service-key authentication, rate limiting and usage checks. A short
voucher consumes none of the shared service key's rate limit. Management actions bypass
those checks entirely, after the same body pre-flight and credential verification.

The chat completion's usage observer reports final counts once. Non-streaming completions
normally report before the handler returns; if no report arrives within one second after a
successful response, the wrapper releases the reservation and records `usage_missing`.
Streaming completions charge asynchronously on the final report, or release on an incomplete
report (including a dropped stream). A missing stream report is bounded at fifteen minutes.
An inner error, including a rate-limit `429`, releases the reservation. Missing or closed
channels return `410 application/problem+json`; voucher and minimum-delta refusals return
`402` with a fresh challenge carrying the authenticated session snapshot.

### Channel checks

`open`: decode the signed `0x76` transaction; exactly one call to the escrow with selector
`open`, `operator` zero, `payee` equal to our signer, `token` equal to `MPP_TEMPO_CURRENCY`,
deposit positive; recover the payer from the signing hash; descriptor and channel id must match
the credential; the opening voucher must be `<= deposit` and verify for the channel's voucher
signer; broadcast, require receipt status success, read chain state (`deposit > 0`, no close
requested), insert or refresh the channel row.

`topUp`: same decoding rules with `topUp`; descriptor must match the stored channel;
`additionalDeposit` must match; refresh `deposit` from chain (never decrease stored deposit or
settled).

`voucher`: channel must exist and accept vouchers (`open`, no close requested on chain);
descriptor must match; cumulative `>= settled_on_chain`, `<= deposit`, strictly above the
stored highest (or an exact replay of the same highest amount and signature bytes); signature
verifies (secp256k1, 65 bytes); delta from the previous highest must be at least
`min_voucher_delta`. On-chain state is read at most once every 60 seconds per channel to
refresh `deposit`, `settled`, and `closeRequestedAt`; zero deposit means closed, so refuse.
Once the payer has requested close on chain, no further vouchers or reservations are accepted.

`close`: voucher checks as for inference (amount may equal the highest accepted); payer or
voucher signer must supply `closeSignature` over `CloseAuthorization`; capture is
`max(spent, settled_on_chain)` with `cumulativeAmount >= capture`; channel moves to `closing`,
then the payee broadcasts `close` on chain.

### Settlement

The ledger keeps the highest voucher and up to sixteen earlier vouchers. A voucher is
settleable only when `settled_on_chain < cumulative <= spent - reserved` (never settle above
what was delivered). When `spent - last_settlement_spent >= MPP_SETTLE_AMOUNT` or
`now - last_settlement_at >= MPP_SETTLE_INTERVAL_SECS` and a settleable voucher exists, one
instance signs and broadcasts `settle` for that voucher; on success it updates
`settled_on_chain` and drops retained vouchers at or below the settled amount. At most one
settlement run holds the channel at a time (60 second lease on the row).

### Close supervision

Every `MPP_CLOSE_SUPERVISION_SECS` (default 60), for channels not yet finalized, the server
reads escrow state in batch. If `closeRequestedAt` is set on chain, it records that, stops
new vouchers, and, when `spent - reserved` exceeds `settled_on_chain`, submits a payee close
with capture equal to delivered spend, using a voucher at least that high, inside the 900 second
grace period. Channels whose on-chain deposit is zero are marked finalized (withdrawn). Stale
reservations with no finish are released after fifteen minutes. Finalized channel rows are purged
after thirty days.

### Session configuration

When any model has `token_price`, `MPP_TEMPO_SIGNER_KEY` / `_FILE` is required: a 32-byte
hex secp256k1 key whose address is the session `recipient` and signs settlement and payee-close
transactions. Fund that address with the fee token (`MPP_TEMPO_CURRENCY`) for gas on settle and
close, in addition to whatever treasury policy you use for `charge`.

| Variable | Meaning |
|---|---|
| `MPP_TEMPO_SIGNER_KEY` / `_FILE` | Payee settlement signer (required if any `token_price`) |
| `MPP_SESSION_MIN_VOUCHER_DELTA` | Minimum voucher step in base units (default `token_price × 64`) |
| `MPP_SESSION_SUGGESTED_DEPOSIT` | Suggested opening deposit (default `1000000`) |
| `MPP_SETTLE_INTERVAL_SECS` | Settlement age threshold (minimum `60`, default `3600`); task polls every quarter interval, at least `60` seconds |
| `MPP_SETTLE_AMOUNT` | Settle when delivered spend since last settlement reaches this (default `5000000`) |
| `MPP_CLOSE_SUPERVISION_SECS` | Close-supervision poll interval (`10` to `300`, default `60`) |

These share the enabled MPP settings above: `MPP_REALM`, `MPP_SECRET_KEY` / `_FILE`,
`MPP_SERVICE_API_KEY` / `_FILE`, `MPP_CHALLENGE_TTL_SECS`, `MPP_TEMPO_RPC_URL`,
`MPP_RPC_TIMEOUT_SECS`, `MPP_TEMPO_CHAIN_ID`, `MPP_TEMPO_CURRENCY`,
`MPP_TEMPO_RECIPIENT` and `MPP_MODEL_PRICES`. The treasury recipient remains the charge
recipient; sessions use the signer address. The escrow precompile is fixed, with no extra
environment variable. With `session: None` (no model has `token_price`), no signer, channel
repository or session background task is constructed. Finalized-channel retention runs hourly
alongside claim retention, deleting rows last updated more than thirty days ago.

Example model entry with both intents:

```json
{"zai-org/GLM-5.1-FP8": {"amount": "20000", "token_price": "8", "max_tokens": 2048, "max_request_bytes": 65536}}
```

### Session operator setup

1. **Signer and escrow.** Generate and protect `MPP_TEMPO_SIGNER_KEY`; its address is every
   session challenge's `recipient`. Keep enough of `MPP_TEMPO_CURRENCY` on that address to pay
   Tempo fees for periodic `settle` and payee `close` transactions.
2. **Schema.** Channel state lives in `mpp_channels` and in-flight holds in
   `mpp_channel_reservations` (`V0100__mpp_channels.sql` in `nearai/cloud-platform`). Deploy
   that migration and bump `scripts/cloud-platform-schema.env` before enabling session pricing.
   `mpp_channels` holds the channel descriptor, on-chain deposit and settled amounts, highest
   and retained vouchers, `spent`, `reserved`, token `units`, lifecycle state, settlement and
   close metadata, and timestamps. `mpp_channel_reservations` holds `request_id`, `channel_id`,
   reserved `amount`, and `created_at`. Neither table stores organization, workspace, request
   content, or API key.
3. **Reconciliation.** On-chain `settle` and payee `close` move value to the payee (your
   signer) and refund the payer's remainder. Compare escrow events and the signer's balance
   with `units` and settlement timestamps on the channel rows, alongside the service workspace
   usage you already watch for `charge`.

Session storage follows the same privacy rules as `charge`: no credentials, signatures, channel
ids, or payer addresses in logs or metrics; problem details stay generic.

### Session limits

- Signatures are secp256k1 only (65-byte `r||s||v`); P-256 and WebAuthn (TIP-1020) are refused.
- No fee sponsorship (`feePayer`), no machine token, no auto-swap transaction prefix, no
  mid-stream voucher updates (one voucher per request before inference).
- One currency, one chain, one session recipient (the signer), `POST /v1/chat/completions` only.
- NEAR Intents is offered on `charge` only, not on `session`.

### Testing against Tempo Moderato with mppx

On chain id `42431`, use mppx's session manager so the client opens the channel, signs
vouchers, and handles `sessionSnapshot` top-ups:

```ts
import { Mppx, tempo } from 'mppx/client'
import { privateKeyToAccount } from 'viem/accounts'

const account = privateKeyToAccount(process.env.TEMPO_PRIVATE_KEY as `0x${string}`)
Mppx.create({
  methods: [tempo.session.manager({ account, maxDeposit: 1_000_000n })],
})
```

Point `MPP_TEMPO_RPC_URL` at Moderato and list `token_price` on the models you test.

For a standalone endpoint with the real RPC and signer, and in-memory ledgers, run the live
harness from the repository root. Set `MPP_TEMPO_SIGNER_KEY_FILE` to a protected file holding
the funded payee's hex key, then export:

```sh
export MPP_TEMPO_RPC_URL=https://rpc.moderato.tempo.xyz
export MPP_TEMPO_CHAIN_ID=42431
export MPP_TEMPO_CURRENCY=0x20c0000000000000000000000000000000000000
export MPP_MODEL_PRICES='{"local":{"token_price":"2","max_tokens":256,"max_request_bytes":8192}}'
cargo run -p api --features services/test-mocks --example mpp_session_live
```

The process prints `PORT=<n>` and listens on `127.0.0.1:<n>`. Point the unchanged mppx client
at `http://127.0.0.1:<n>/v1/chat/completions` with model `local`, `max_tokens: 64`, and a valid
chat body. The stub reports seven prompt tokens and five completion tokens through the real
observer, so a completed request costs twenty-four base units at that price. Management
receipts come from the real chain. The harness loses its ledgers on exit and starts no
settlement or supervision workers; use the production server to exercise those tasks.
