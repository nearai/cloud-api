# Chutes public-key routing

After fetching and verifying a Chutes attestation report, an SDK can constrain a
chat request to its `e2e_pubkey` by sending:

```http
POST /v1/chat/completions
X-Model-Pub-Key: <the report's e2e_pubkey>
```

Use the same model as the verified report. The Chutes key is standard base64
encoding of an ML-KEM-768 public key (1184 decoded bytes); preserve its case and
padding. Existing NEAR hexadecimal routing keys remain supported.

The gateway selects candidates for that model with the exact requested key,
performs the existing attestation checks, encrypts to that key, and invokes the
selected candidate with `X-Instance-Id`. Multiple instances can share a key: this
constraint identifies the verified key, not a unique physical CVM.

The constraint survives discovery refreshes and retries for both streaming and
non-streaming chat. If no matching instance is available, the request fails; it
does not fall back to NEAR or another key. After key rotation, the SDK must fetch
and verify a fresh report before choosing a new key. Streaming remains subject
to the existing `CHUTES_ENABLE_STREAMING` setting.

Routing alone requires no client encryption headers. This does not add
client-to-Chutes E2EE or model response signatures: the existing encrypted
gateway-to-Chutes channel is unchanged. Requests without `X-Model-Pub-Key` retain
their normal provider selection.
