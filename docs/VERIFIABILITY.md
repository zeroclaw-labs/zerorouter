# Verifiability: making "we don't store your data" checkable

ZeroRouter reads customer prompts in plaintext. It must — metering and
routing require the request body — and that makes its retention promise a
trust claim: nothing a customer can observe distinguishes an honest
deployment from one that logs every prompt. Open-sourcing the code does not
close that gap. Source proves what *could* run, not what *does* run, and a
tap does not have to live in the codebase at all: TLS terminates at the load
balancer today, so a packet capture on the host, a listener at the ALB, or a
quietly modified binary would each see plaintext while presenting exactly the
same outside behavior as the honest system.

This document is the staged plan for closing that gap as far as current
technology allows, and — just as important — an honest statement of what each
stage does and does not prove. Overclaiming here would be worse than the gap
itself.

## The endpoint

`GET /transparency` (public, unauthenticated) reports what the running
deployment claims to be:

- `source_commit` — baked into the image at build time
  (`ZEROROUTER_SOURCE_COMMIT`), with a browsable `source` link;
- `image` / `image_digest` — what the ECS container metadata endpoint says
  this task is actually pinned to;
- `attestations` — the public record for that digest;
- `verify` — the one-line check;
- `caveat` — the scope of the claim, stated where the claim is made.

## Stage 1 (shipped): build provenance

Every image the deploy workflow builds is **attested**: GitHub artifact
attestations produce a Sigstore signature, recorded in GitHub's public
attestation store, binding the image digest to the exact public commit and
the exact workflow run that built it. The signing identity is the workflow's
OIDC identity — not a key any human holds — and the store is public:

```console
# With the gh CLI (and pull access to the registry):
gh attestation verify oci://<image>@<digest> --repo zeroclaw-labs/zerorouter

# Without any registry access — the attestation bundle is public HTTPS:
curl -s https://api.github.com/repos/zeroclaw-labs/zerorouter/attestations/<digest>
```

The walkable chain is:

```text
running service → /transparency → image digest
    → signed attestation (Sigstore, public store)
    → public commit + public workflow run
    → the source you can read
```

**What stage 1 proves.** The digest the deployment cites was built by the
public deploy workflow, from the public source, at the named commit, without
modification. Nobody — including the operator — can forge that binding after
the fact; producing it requires committing the change to the public
repository first. Any claim the deployment makes about its own behavior is
therefore a claim about code anyone can read.

**What stage 1 deliberately does not prove.**

1. **That the host runs the attested image.** `/transparency` is served by
   the binary itself; a malicious operator's binary could recite an honest
   build's digest. The ECS metadata source raises the effort but is read
   through the same binary.
2. **That nothing observes plaintext outside the process.** TLS still
   terminates at the ALB. A listener between the ALB and the container — the
   exact concern that motivates this document — is invisible to stage 1.
3. **That the upstream providers store nothing.** ZeroRouter forwards
   requests to model providers; their retention is governed by their
   zero-retention agreements, not by anything this repository can attest.

Stage 1's honest summary: *the code is provably the public code; the
runtime is still trusted.* That is a real reduction in what a customer must
take on faith — retention behavior becomes a property of auditable public
source rather than of a private binary — and it is the substrate the later
stages verify against.

## Stage 2 (planned): remote attestation — the runtime stops being trusted

Confidential-computing hardware (AWS Nitro Enclaves; AMD SEV-SNP / Intel TDX
confidential VMs) closes gaps 1 and 2 structurally rather than
procedurally:

- **Measurement.** The hardware hashes the image actually booted inside the
  enclave and signs that measurement with a key rooted in the platform
  vendor. The operator cannot forge it: "what is running" becomes a
  hardware-signed fact instead of a self-report.
- **Attested TLS.** The TLS private key is generated inside the enclave,
  never exists outside it, and is bound to the measurement. A client (or
  SDK) verifies the attestation **before** sending data, and the session
  terminates inside the measured code. Everything on the line outside the
  enclave — ALB, host, hypervisor, the operator with root — carries only
  ciphertext. This is the direct answer to the listener: there is no point
  in the path where plaintext exists outside code whose hash is signed by
  hardware.
- **Encrypted memory.** Enclave RAM is unreadable from the host, closing
  the memory-dump variant.

Combined with stage 1, the chain extends to: *the hardware attests the
running measurement; the measurement resolves (via the attestation store) to
a public commit; the public source at that commit provably does not persist
content.* At that point "we don't store your data" is no longer a promise —
it is a checkable property of the deployment, modulo the caveats below.

Residual trust and open engineering, stated plainly:

- **Trust moves to the silicon vendor** (AWS/AMD/Intel root of trust) and to
  the absence of side-channel escapes — a real, historically active attack
  surface. Far smaller than "trust the operator"; not zero.
- **Reproducible builds become load-bearing.** An attested measurement of an
  image nobody can rebuild proves an opaque blob is running. The Rust binary
  and the distroless image need bit-reproducibility (or at minimum,
  independently re-buildable provenance) before the measurement means what
  it should. This is real work and can start any time; stage 1 does not
  require it, stage 2 does.
- **Infrastructure change**: Fargate does not support enclaves, so stage 2
  means EC2 Nitro Enclave hosts (or confidential VMs elsewhere), vsock
  plumbing between the instance and the enclave, KMS keys released only
  against the expected measurement, and an attestation document endpoint.
  Multi-week project; belongs to the infrastructure repository as much as to
  this one.
- **Metadata is out of scope.** Timing, sizes, model choice, and source
  addresses remain visible to the operator at every stage. Attestation hides
  content, not traffic shape.
- **The upstream leg stays contractual** (gap 3). Attestation can prove the
  gateway forwards to the provider endpoints the source names and nothing
  else — it cannot reach into the provider. Provider-side zero-retention
  terms remain part of the honest claim.

## Stage 3 (planned): client-side verification

Attestation nobody checks is theater. The final stage teaches the clients to
verify before they send: the SDK / key-issuance flow pins the expected
measurement (or its transparency-log inclusion), refuses to send to an
unattested endpoint, and surfaces the verified commit to the user. Apple's
Private Cloud Compute is the shipped precedent for the full shape —
attestation, a public transparency log of measurements, stateless nodes, and
external researcher access — and is the reference point this plan converges
toward, scaled to a rather smaller fleet.

## Non-goals

- **FHE / MPC inference.** Computing on encrypted prompts without a TEE is
  orders of magnitude too slow for LLM traffic today. If that changes, it
  obsoletes stage 2 happily.
- **Proving a negative with logs, audits, or policy.** SOC 2 and friends
  have organizational value; they are not evidence of absence and are not
  part of this chain.
- **Hiding traffic metadata from the operator.** Out of scope at every
  stage; saying otherwise would be the overclaim this document exists to
  avoid.
