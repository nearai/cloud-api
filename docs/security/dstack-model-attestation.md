# Model attestation with dstack

A successful model report from a proxy using dstack Attest includes the hex-encoded `dstack_attestation`. The model verifier accepts the `tdx` and `sev-snp` evidence for a dstack guest in SCALE V0 or MessagePack V1. During a rolling upgrade, an older Intel proxy can still supply just `intel_quote` and `event_log`. If the generic field is present, verification must succeed with that evidence. The public report forwards the fields supplied by each model.

For Intel TDX, the verifier checks the DCAP quote and replays the runtime events against RTMR3. For AMD SEV-SNP, it checks the hardware report, the AMD certificate chain and current revocation list, and the configured launch and firmware policy. Both paths bind the signing address, TLS fingerprint and request nonce to the hardware report and use the existing GPU policy. A replica signing key from an RTMR3 runtime event is available on TDX. SNP runtime events carry no equivalent hardware binding.

## Configuring SNP admission

Set `DSTACK_SNP_POLICY_FILE` to a local JSON file containing both arrays below. The verifier loads it on first use. Restart the service after changing it. Intel verification works without this setting.

| Field | Value |
| --- | --- |
| `launches[].measurement` | The 48-byte expected SNP launch measurement, in hex. Compute it independently from the approved image and launch parameters. |
| `launches[].os_image_hash` | The 32-byte dstack OS image digest, in hex, for that measurement. The existing image allowlist also applies when configured. |
| `launches[].compose_hash` | The approved 32-byte compose hash in hex. The signed report must bind the exact MrConfigV3 document containing it through HOST_DATA. |
| `minimum_tcbs[].cpuid` | Eight hex digits for the CPU's CPUID Fn0000_0001 EAX, including stepping. |
| `minimum_tcbs[].bootloader`, `tee`, `snp`, `microcode` | Minimum component security versions from the applicable AMD bulletins, as integer bytes. |
| `minimum_tcbs[].fmc` | Minimum FMC security version for processors with this component. Required for Turin. |

Each accepted measurement and compose combination must have a matching launch entry. The OS image identity comes from that entry. The reported, committed and launch TCBs must all satisfy the CPU-specific minimum. A missing policy, an unlisted CPU or unavailable revocation data prevents SNP admission. [dstack launch measurements and TCB requirements](https://github.com/Dstack-TEE/dstack/blob/v0.6.6/docs/amd-sev-snp.md#attestation-and-image-identity)
