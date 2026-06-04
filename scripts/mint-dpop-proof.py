#!/usr/bin/env python3
"""Mint a DPoP proof JWT signed by an ES256 keypair.

Used by the host probes (test-persistence.sh, test-cold-start.sh,
test-sse.sh's prepare setup) to talk to the now-DPoP-bound
`POST /x402/v2/prepare` and `POST /x402/v2/settle` endpoints.

Two modes:

- `--init <keyfile>`: generate a fresh ES256 keypair and write it as
  PKCS#8 PEM to `<keyfile>`. Probe runs that share one keypair across
  multiple calls (so the JKT stays stable for idempotency) generate
  the key once at startup, then reuse it.

- (default) `--keyfile <keyfile> --method <verb> --url <url> --jti
  <jti>`: read the PEM, mint a DPoP proof for `(method, url, jti)`
  with `iat=now`, and print the proof JWT to stdout.

Requires: PyJWT >= 2.10, cryptography >= 41. Both are typically
present on dev machines via `pip install pyjwt cryptography`.

This script is deliberately small and dep-light. It does not aim to be
a full DPoP library; it is the equivalent of an ssh-keygen for tests.
"""

import argparse
import base64
import hashlib
import json
import os
import sys
import time

try:
    import jwt as pyjwt
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ec
except ImportError as err:
    sys.stderr.write(
        f"[mint-dpop-proof] missing dependency: {err}; install with `pip install pyjwt cryptography`\n"
    )
    sys.exit(2)


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def init_keypair(path: str) -> None:
    key = ec.generate_private_key(ec.SECP256R1())
    pem = key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    with open(path, "wb") as fh:
        fh.write(pem)
    os.chmod(path, 0o600)


def load_keypair(path: str):
    with open(path, "rb") as fh:
        return serialization.load_pem_private_key(fh.read(), password=None)


def jwk_from_key(key) -> dict:
    pub = key.public_key().public_numbers()
    x = pub.x.to_bytes(32, "big")
    y = pub.y.to_bytes(32, "big")
    return {
        "kty": "EC",
        "crv": "P-256",
        "x": b64url(x),
        "y": b64url(y),
    }


def jkt_for(jwk: dict) -> str:
    canonical = json.dumps(
        {"crv": jwk["crv"], "kty": jwk["kty"], "x": jwk["x"], "y": jwk["y"]},
        separators=(",", ":"),
        sort_keys=False,
    ).encode("utf-8")
    return b64url(hashlib.sha256(canonical).digest())


def mint(keyfile: str, method: str, url: str, jti: str) -> str:
    key = load_keypair(keyfile)
    jwk = jwk_from_key(key)
    headers = {"typ": "dpop+jwt", "alg": "ES256", "jwk": jwk}
    claims = {"htm": method, "htu": url, "jti": jti, "iat": int(time.time())}
    pem = key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    return pyjwt.encode(claims, pem, algorithm="ES256", headers=headers)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--init", metavar="KEYFILE", help="generate a new keypair PEM")
    parser.add_argument("--keyfile", help="path to existing PKCS#8 PEM keypair")
    parser.add_argument("--method", default="POST", help="HTTP verb (htm)")
    parser.add_argument("--url", help="request URL (htu)")
    parser.add_argument("--jti", help="proof jti (must be unique per request)")
    parser.add_argument(
        "--print-jkt",
        action="store_true",
        help="print the keypair's RFC 7638 JWK thumbprint to stdout",
    )
    args = parser.parse_args()

    if args.init:
        init_keypair(args.init)
        return 0

    if not args.keyfile:
        parser.error("--keyfile is required unless --init is used")

    if args.print_jkt:
        key = load_keypair(args.keyfile)
        print(jkt_for(jwk_from_key(key)))
        return 0

    if not (args.url and args.jti):
        parser.error("--url and --jti are required when minting a proof")

    print(mint(args.keyfile, args.method, args.url, args.jti))
    return 0


if __name__ == "__main__":
    sys.exit(main())
