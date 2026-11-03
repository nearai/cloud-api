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

Metered per-token billing is the MPP `session` intent. It is a follow-up, not part of this
change.

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
  credentials, no `session` intent.
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
