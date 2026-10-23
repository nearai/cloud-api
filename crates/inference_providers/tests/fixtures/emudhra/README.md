# eMudhra TLS certificate fixtures

These are public certificates captured on 2026-10-20. The 12 files in
`chains/` are the served leaf-first chains from
https://github.com/galadran/we-have-a-problem.com/tree/main/chains. The
`roots/` files contain the two formerly trusted eMudhra/AAA anchors from
Debian bookworm `ca-certificates 20230311+deb12u1` and GTS Root R4 for the
healthy `api.openai.com` control chain.

The fixtures contain public certificate material only and are safe to keep in
the repository.
