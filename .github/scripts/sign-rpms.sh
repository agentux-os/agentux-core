#!/bin/sh
# Signs the RPMs given as arguments with the AgentUX RPM signing key, and
# writes its public half to RPM-GPG-KEY-agentux next to the first RPM.
#
#   AGENTUX_RPM_SIGNING_KEY         the armored private key (a secret)
#   AGENTUX_RPM_SIGNING_PASSPHRASE  its passphrase, if it has one (a secret)
#
# Without the key it signs nothing, says so, and exits 0: releases keep
# working until the key exists (docs/releasing.md). Runs in Fedora 44.
set -eu

if [ -z "${AGENTUX_RPM_SIGNING_KEY:-}" ]; then
    echo "::warning::AGENTUX_RPM_SIGNING_KEY is not set: the RPMs are not signed (docs/releasing.md)"
    exit 0
fi
dnf -y install --setopt=install_weak_deps=False rpm-sign gnupg2 >/dev/null

export GNUPGHOME
GNUPGHOME=$(mktemp -d)
trap 'rm -rf "$GNUPGHOME"' EXIT
printf '%s\n' "$AGENTUX_RPM_SIGNING_KEY" | gpg --batch --quiet --import
fingerprint=$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" { print $10; exit }')
[ -n "$fingerprint" ] || { echo "::error::AGENTUX_RPM_SIGNING_KEY holds no secret key"; exit 1; }

extra="--batch --pinentry-mode loopback"
if [ -n "${AGENTUX_RPM_SIGNING_PASSPHRASE:-}" ]; then
    printf '%s' "$AGENTUX_RPM_SIGNING_PASSPHRASE" >"$GNUPGHOME/passphrase"
    extra="$extra --passphrase-file $GNUPGHOME/passphrase"
fi
for rpm in "$@"; do
    rpmsign --addsign \
        --define "_gpg_name $fingerprint" \
        --define "_gpg_sign_cmd_extra_args $extra" \
        "$rpm"
done

out="$(dirname "$1")/RPM-GPG-KEY-agentux"
gpg --batch --armor --export "$fingerprint" >"$out"
# Prove it: the signatures check against the exported public key alone.
check=$(mktemp -d)
rpm --dbpath "$check" --import "$out"
for rpm in "$@"; do
    rpm --dbpath "$check" -K "$rpm" | grep -q 'signatures OK' \
        || { echo "::error::$rpm does not verify"; exit 1; }
done
rm -rf "$check"
echo "signed $# RPM(s) with $fingerprint"
