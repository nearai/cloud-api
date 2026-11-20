# Offline APT bundle

The root Dockerfile uses the immutable `image` reference in `bundle.lock.json`.
This locally assembled OCI image contains `/apt-bundle`: original signed Debian
and Debian Security repository metadata plus the 19 exact downloaded package
versions in `debian-packages.txt`, for snapshot `20260928T000000Z` and amd64. The
manifest is the combined builder/runtime download closure; inherited base
packages, including already installed builder dependencies, need no extra deb. The
original root package pin files remain authoritative for each install.

`COPY --from=apt-bundle` exposes data in a scratch stage; read-only RUN mounts
keep repository contents out of application layers. Both APT installs run with
`--network=none`. The independently reviewed, source-controlled `prepare-apt.sh`
is included here and is never loaded from the bundle. Its SHA256 is recorded in
the lock. It creates isolated file-only APT configuration, applies the original
pins at priority 1001, and uses the pinned base image's
`/usr/share/keyrings/debian-archive-keyring.gpg` as the trust anchor. APT verifies
repository signatures and package checksums; no trust key comes from the bundle.
Base automatic-package state is copied into the isolated APT state and written
back after installation; temporary configuration/state is then deleted in the RUN.
Runtime CA revocation checks and reproducible cleanup remain mandatory.

For an update, regenerate the exact download closure from the selected signed
snapshot and unchanged base/pins, assemble the OCI data image using the separately
reviewed shared tooling, verify its manifest digest and package inventory, and
publish it. Update the Dockerfile, manifests and lock together. CI/release review
must compare the source manifest against the artifact, verify signed indexes and
package checksums using independent keys, anonymously pull the public digest, and
run full application reproducibility and existing package-pin comparison checks.
There is no automatic publisher or secret-handling workflow in this repository.

Helper integration is complete; publication remains pending. Do not deploy this
candidate before public anonymous bundle pull and full reproducibility
verification pass. This changes build inputs and requires a new Brave-approved
application digest; it does not preserve or approve any previous application
digest. Existing `build-image.sh` options remain unchanged.
