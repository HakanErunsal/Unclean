# Security policy

Unclean has no published release. Report security flaws found in the source or build artifacts.

## Private reporting

Use GitHub private vulnerability reporting or a private security advisory for this repository.
Do not open a public issue for a flaw that could corrupt files, cross the privilege boundary,
write outside a selected target, disclose private data, or substitute release binaries.

If no private reporting channel is available, open a public issue asking a maintainer to provide
a private contact. Do not include exploit details, private paths, descriptor contents, logs, or
personal data in that issue.

Include the affected revision, operating system, reproduction conditions, observed result,
expected result, and the smallest synthetic proof of concept that demonstrates the flaw.

## Priority areas

Reports are especially useful when they involve:

- path traversal, junctions, symlinks, or root validation;
- stale or tampered elevated requests;
- writes to fields or file types outside the declared authority;
- backup, replacement, verification, rollback, or restore failures;
- parser crashes, resource exhaustion, or malformed input;
- logs or reports that expose descriptor contents or private paths;
- dependency compromise, build provenance, signing, or release substitution.

## Disclosure

Maintainers will confirm receipt, investigate against the reported revision, and coordinate a
public advisory when users need to act. Exact timing depends on impact, reproduction, and release
readiness. Keep technical details private until a fix or mitigation is available.
