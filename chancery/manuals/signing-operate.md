# Configure Cell storage and native signing

Telete owns Cell's shared current-user workspace and signing selections. These
commands do not need an initialized delivery queue. Signing maintenance checks
an existing selected Telete queue before changing policy.

## Set up the host

Use the built Telete executable for first setup. Installation itself requires
configured storage, an initialized queue, and a usable signing identity.

```sh
telete storage configure --volume /Volumes/CellWork
telete storage status
telete signing create-local
telete signing status
```

Storage setup runs before queue paths are opened. It writes the shared
current-user `~/Library/Application Support/Cell/workspace.json`, with
`schema_version: 1`, the exact mount path and `volume_uuid`, and
`directory: "cell"`. The account database selects the home directory; `HOME`
and `--state` do not redirect this host file. The volume must be mounted,
external, writable APFS with ownership enabled. Setup creates a private
mode-0700 `cell` directory and publishes the mode-0600 selector without
replacing an existing file. Repeating an identical valid selection succeeds
without rewriting it. A different selection fails. Status reads the selection
and current volume observation without creating queue state.

An unavailable volume never selects a new queue. Restore the selected volume
and its identity. Storage setup does not move data or select another accepted
baseline.

Initial signing creates the shared Cell identity. It writes
`~/Library/Application Support/Cell/signing.json`, not a Telete state override.
Creation requires configured storage but no initialized queue. It refuses an
existing shared or selected Telete policy, or an existing `Cell Local Signing`
certificate in the login Keychain. An invalid selection is not permission to
create a replacement.

The certificate uses RSA 3072, SHA-256, 3,650-day validity, and critical
`CA:FALSE`, digital-signature, and code-signing extensions. Setup imports the
certificate and key into the current user's login Keychain, authorizes
`/usr/bin/codesign` for key use, and adds user code-signing trust. Native user
authorization can be required. The SHA-1 fingerprint selects the exact
certificate; it is not the certificate signature algorithm. The schema-one
policy uses the `local` profile and `local.cell` namespace.

Shared signing writers hold the host setup lock and the selected Telete state's
admission, worker, and deployment locks, plus the retained legacy manual-deployment lock.
The queue must be paused and drained
when present. Stop the worker before maintenance. Release publication and
retained Telete recovery and legacy manual deployment effects must be settled. Settle any other
independently
configured Telete states before changing a shared selection; these commands
check the selected state, not an inventory of all consumers. The commands do
not pause, stop, cancel, or recover work for the operator.

Private generation files are removed on ordinary completion or failure. Only
Keychain retains the key. Setup preflights the identity before publishing the
policy. Keychain changes and file publication are not one transaction: a failure
or timeout can leave an imported certificate or key without configuration.
Inspect the reported fingerprint and Keychain effects. Do not delete or
regenerate the identity to repeat creation. Once that exact key is usable,
select it explicitly with:

```sh
telete signing configure --host --certificate-sha1 FINGERPRINT \
  --keychain /absolute/login.keychain-db --identifier-namespace local.cell
```

This command also supports deliberate shared identity changes under the same
maintenance guards. A failed preflight preserves the prior policy. If key
import did not complete, configuration alone cannot restore the missing key.
An abrupt process termination can also leave private generation staging for
explicit inspection and cleanup. No automatic renewal, identity replacement,
volume formatting, mounting, ownership change, or journal migration is supplied.

## Use the signing policy

Telete owns the shared Cell signing policy. Builds, repairs, cache resets,
reinstallations, and upgrades do not select or generate another certificate.
The supported `local` profile supplies no Developer ID distribution,
notarization, installer-package signing, Hardened Runtime, additional
entitlements, or macOS permission grant. Signing continuity does not promise
that macOS will never request permission again.

Run `telete signing status` to read the selected policy and probe its exact
certificate, private key, and code-signing usability through macOS Security and
a dry-run signature. The command changes no policy or product selection.
The result includes `policy`, `ready: true`, and `selection`, which identifies
the host policy or Telete override. Readiness describes that observation only.
Missing configuration, an inaccessible Keychain, or an unavailable or expired
identity fails with exit 1 and a diagnostic. CLI syntax errors use exit 2.
Add `--json` for compact JSON output and structured failure diagnostics.

Prepare a deliberate identity change:

1. Run `telete pause` to stop new claims.
2. Inspect `telete status` and settle all active and queued jobs. Pause alone
   does not drain them. Cancel jobs only when abandonment is intended.
3. Stop the worker and settle Telete recovery, legacy manual deployment effects, and release
   publication.
4. Unlock the intended Keychain and permit the requested key use through macOS.
5. Run `telete signing configure --host` with the intended identity.
6. Inspect `telete signing status` before starting the worker and resuming.

The fingerprint must contain exactly 40 hexadecimal digits. Telete normalizes
it to lowercase. The Keychain path must be absolute and name a regular file
owned by the current user. The namespace defaults to `local.cell` and must be
a dotted lowercase identifier. Certificate-name matching, per-product identity
overrides, and environment signer overrides are unsupported.

The shared configuration uses this format:

```json
{
  "schema": 1,
  "macos": {
    "profile": "local",
    "certificate_sha1": "<40 lowercase hexadecimal digits>",
    "keychain": "/absolute/login.keychain-db",
    "identifier_namespace": "local.cell"
  }
}
```

The configuration is an owned mode-0600 file in a private mode-0700 directory.
It contains no private-key bytes or password. Configuration preflights the
requested identity before atomic publication. A failed preflight preserves
the prior file. Omitting `--host` writes a Telete state override, which retains
precedence over the shared policy. Status identifies which policy it used.

Changing the fingerprint selects another certificate. Changing the namespace
changes product code identifiers. Changing only the Keychain path locates the
same certificate in another Keychain. Reissuing a certificate with the same
private key still changes its fingerprint and requires explicit selection.
An unavailable selected identity remains blocked until it becomes usable or
the operator explicitly configures another identity.

New jobs freeze the policy at submission. Reusing a request key retains that
snapshot. Preparation and deployment reject a changed selection. The permanent
executable key is the declared release-binary name. The code identifier is
`NAMESPACE.PRODUCT.EXECUTABLE`; the product formerly named `decisions` uses
canonical identity `krisis`. Versions, build paths, Git commits, and release
directories do not enter the identifier. The designated requirement pins this
identifier and the exact leaf certificate.

The policy covers Cell-owned native production commands, daemons, helpers,
and installer executables declared in the inventory. Telete signs fresh staged
copies. Standalone scripts, tests, compiler scratch output, and third-party
runtimes receive no Cell-native process identity. Package assets remain owned
by their product manifests and paths. Signing does not combine products'
permission boundaries.

Signing uses no network timestamp. A successful signing command is packaging
evidence; it does not prove product health, safety, or suitability. Telete retains
the production receipt before source acceptance, including jobs that skip tests,
and checks source, scope, candidate, and policy correlation. Deployment does not
audit signatures before copying or publication. Cargo owns compilation reuse;
Telete retains no additional artifact cache.

Missing keys, expired certificates, locked Keychains, and failed signing commands
stop the operation. There is no unsigned, ad hoc, alternate-name, renewal, or
automatically generated identity fallback. Signing failures do not request
model repair. Repair agents receive no private-key material or configuration
writer.

For initial adoption or rotation, select the intended product inventory with
repeated `--deploy PRODUCT` options and inspect the retained deployment result.
Install Telete separately under its maintenance procedure. Product installation
is sequential; a failed rollout can leave earlier releases selected. Inspect
each result before claiming that the inventory uses one signer. Immutable
releases are not re-signed in place. Prepare historical source as a new release
under the current policy, and request program recovery explicitly.

The creation lifetime does not promise perpetual key usability. No automatic
renewal, atomic fleet rotation, historical signature audit, retro-signing,
private-key escrow, completion deadline, or continual readiness audit is supplied.
The current-user boundary does not isolate the key from hostile code already
running with that user's authority. Production candidates and installed releases
use opaque UUID identities.
