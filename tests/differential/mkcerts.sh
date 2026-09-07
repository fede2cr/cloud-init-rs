#!/bin/sh
# Build the certificate set the `https` differential cases need.
#
# One CA, three server certificates that differ only in whether a client should
# accept them, and one client certificate for the mutual-TLS case. Everything is
# thrown away with the work directory; nothing here is ever installed.
set -e

out="$1"
mkdir -p "$out"
cd "$out"

quiet() { "$@" >/dev/null 2>&1; }

quiet openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem \
    -days 1 -subj "/CN=cloud-init-rs differential CA" \
    -addext "basicConstraints = critical, CA:TRUE" \
    -addext "keyUsage = critical, keyCertSign, cRLSign"

# The same CA without the key usage RFC 5280 requires of one. Anything
# verifying strictly refuses to build a chain through it.
quiet openssl req -x509 -newkey rsa:2048 -nodes -keyout sloppyca.key \
    -out sloppyca.pem -days 1 -subj "/CN=cloud-init-rs sloppy CA" \
    -addext "basicConstraints = critical, CA:TRUE"

# A certificate signed by the CA above, with the given subject alternative name.
signed() {
    name="$1"
    san="$2"
    ca="${3:-ca}"
    printf 'subjectAltName = %s\nbasicConstraints = CA:FALSE\n' "$san" >"$name.ext"
    quiet openssl req -newkey rsa:2048 -nodes -keyout "$name.key" \
        -out "$name.csr" -subj "/CN=$name"
    quiet openssl x509 -req -in "$name.csr" -CA "$ca.pem" -CAkey "$ca.key" \
        -CAcreateserial -out "$name.pem" -days 1 -extfile "$name.ext"
}

# Matches the address the harness connects to.
signed server "IP:127.0.0.1"
# Signed by the trusted CA, but for somebody else.
signed wrongname "DNS:wrong.invalid"
# For the mutual-TLS case; the server reports the CN it saw.
signed client "DNS:client.invalid"
# Correct in every way except the CA that issued it.
signed sloppy "IP:127.0.0.1" sloppyca

# One file holding both halves. `fetch_ssl_details` reporting a `cert.pem` and a
# `key.pem` separately is unusable upstream (bug B52), so the pair the
# differential drives is the single-file form.
cat client.pem client.key >clientpair.pem

# Right name, no chain to anything the client trusts.
quiet openssl req -x509 -newkey rsa:2048 -nodes -keyout untrusted.key \
    -out untrusted.pem -days 1 -subj "/CN=untrusted" \
    -addext "subjectAltName = IP:127.0.0.1"
