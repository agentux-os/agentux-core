# Releasing

A `v*` tag matching the workspace version builds the `agentux` RPM for
x86_64 and aarch64 in Fedora 44, installs and smoke-tests it, and attaches it
to the GitHub release (`.github/workflows/release.yml`).

## Signed RPMs

The images are signed and installed systems enforce it; the RPMs inside them
are signed too, with the **AgentUX RPM signing key**, once it exists. Until
then releases go out unsigned, with a warning. With the key:

- the release job signs each RPM (`.github/scripts/sign-rpms.sh`, `rpmsign`)
  and checks the signature against the exported public key;
- the verify job imports that key and requires `rpm -K` to say
  `signatures OK` before installing;
- the release carries the public key as `RPM-GPG-KEY-agentux`, and
  agentux-os's image build checks every AgentUX RPM against the copy it ships.

One key signs agentux-core's and agentux-desktop's RPMs. To create it (once,
on a trusted machine, by whoever holds the release secrets):

```sh
export GNUPGHOME=$(mktemp -d)
gpg --batch --passphrase '' --quick-gen-key 'AgentUX RPM signing <release@agentuxos.org>' rsa4096 sign 5y
fpr=$(gpg --with-colons --list-secret-keys | awk -F: '$1 == "fpr" { print $10; exit }')
gpg --armor --export-secret-keys "$fpr" | gh secret set AGENTUX_RPM_SIGNING_KEY -R agentux-os/agentux-core
gpg --armor --export-secret-keys "$fpr" | gh secret set AGENTUX_RPM_SIGNING_KEY -R agentux-os/agentux-desktop
gpg --armor --export "$fpr" > RPM-GPG-KEY-agentux   # commit to agentux-os: files/etc/pki/rpm-gpg/
```

Keep an offline backup of the secret key (`gpg --export-secret-keys`) where
the organization keeps its other release secrets, and remove `$GNUPGHOME`.
A key with a passphrase also needs `AGENTUX_RPM_SIGNING_PASSPHRASE`.
Rotating means a new key in both secrets and the new public key in
agentux-os before the next release.
