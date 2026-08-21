# Test fixtures

## `vertex_test_key.pem`

**This is a throwaway RSA key generated for the test suite. It is not a
credential, it authenticates nothing, and it has never been uploaded to Google
or anywhere else.**

It exists because `src/gcp_auth.rs` signs a JWT with an RSA private key, and the
only way to test that the signature is *correct* — rather than merely
well-shaped — is to verify it against the matching public key. A test that
checked the JWT's structure without verifying the signature would pass against a
signature over the wrong bytes, which is the realistic way that code breaks.

It is committed rather than generated per run because RSA key generation is slow
enough to be felt in a suite this size, and a fixture that changes every run
cannot be reasoned about.

Generated once with:

```bash
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
  -out vertex_test_key.pem
```

If a secret scanner flags this file, that is the scanner working correctly on a
generic pattern — the correct response is to allowlist this path, not to weaken
the test. If you would rather not carry a private key in the repository at all,
the alternative is to generate one in a `OnceLock` at test start and accept the
per-run cost; nothing else in `gcp_auth.rs` depends on the key being stable.
