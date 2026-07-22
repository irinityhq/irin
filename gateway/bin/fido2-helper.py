#!/usr/bin/env python3
"""FIDO2 enrollment + assertion helper for the gateway arm ceremony.

Usage:
  fido2-helper.py enroll [--label LABEL]   Create a resident credential, output registry JSON
  fido2-helper.py assert --challenge HEX   Sign challenge, output confirm JSON

Requires: pip install fido2 (python-fido2 >= 1.0)
Device:   YubiKey or any FIDO2 authenticator over USB HID
RP ID:    gateway.local (hardcoded, matches enrollment)

The sidecar verify path (attest.rs verify_fido2_es256) expects:
  - SEC1 P-256 public key (33 or 65 bytes, base64)
  - DER-encoded ES256 signature (base64)
  - Raw authenticatorData (base64)
  - For native CTAP: clientDataHash = SHA-256(challenge_bytes)
  - For browser WebAuthn (macOS): client_data_json (base64 of the clientDataJSON)
    is accepted by the confirm handler; the server hashes those bytes instead.
"""

import argparse
import base64
import hashlib
import json
import os
import sys
from datetime import datetime, timezone

from fido2.hid import CtapHidDevice
from fido2.ctap2 import Ctap2
from fido2.ctap2.pin import ClientPin, PinProtocolV2
from fido2.webauthn import (
    PublicKeyCredentialParameters,
    PublicKeyCredentialType,
)
from cryptography.hazmat.primitives.asymmetric.ec import (
    EllipticCurvePublicKey,
    SECP256R1,
)
from cryptography.hazmat.primitives.serialization import (
    Encoding,
    PublicFormat,
)

RP_ID = "gateway.local"
RP_NAME = "Gateway Arm Ceremony"

def find_device():
    devs = list(CtapHidDevice.list_devices())
    if not devs:
        print("no FIDO2 device found — insert YubiKey and retry", file=sys.stderr)
        sys.exit(1)
    if len(devs) > 1:
        print(f"multiple FIDO2 devices found ({len(devs)}) — insert only the target key", file=sys.stderr)
        sys.exit(1)
    return devs[0]


def get_pin_token_if_needed(ctap):
    info = ctap.get_info()
    options = info.options or {}
    if not options.get("clientPin"):
        return None, None
    proto = PinProtocolV2()
    client_pin = ClientPin(ctap, proto)
    pin = os.environ.get("FIDO2_PIN")
    if not pin:
        print("FIDO2 PIN required — set FIDO2_PIN env var", file=sys.stderr)
        sys.exit(1)
    try:
        token = client_pin.get_pin_token(pin)
    except Exception:
        token = client_pin.get_pin_token(pin, ClientPin.PERMISSION.MAKE_CREDENTIAL)
    return token, proto


def cmd_enroll(args):
    dev = find_device()
    ctap = Ctap2(dev)

    client_data_hash = os.urandom(32)
    user_id = os.urandom(32)

    print("touch the key when it blinks...", file=sys.stderr)

    pin_token, pin_proto = get_pin_token_if_needed(ctap)

    kwargs = {}
    if pin_token is not None:
        kwargs["pin_uv_param"] = pin_proto.authenticate(pin_token, client_data_hash)
        kwargs["pin_uv_protocol"] = pin_proto.VERSION

    att = ctap.make_credential(
        client_data_hash=client_data_hash,
        rp={"id": RP_ID, "name": RP_NAME},
        user={"id": user_id, "name": "sovereign", "displayName": "Sovereign Operator"},
        key_params=[{"type": "public-key", "alg": -7}],
        options={"rk": True, "up": True},
        **kwargs,
    )

    auth_data = att.auth_data
    cred_data = auth_data.credential_data
    if cred_data is None:
        print("no credential data in response", file=sys.stderr)
        sys.exit(1)

    cred_id_hex = cred_data.credential_id.hex()

    cose_key = cred_data.public_key
    x = cose_key[-2]
    y = cose_key[-3]
    from cryptography.hazmat.primitives.asymmetric.ec import (
        EllipticCurvePublicNumbers,
    )
    pub_numbers = EllipticCurvePublicNumbers(
        x=int.from_bytes(x, "big"),
        y=int.from_bytes(y, "big"),
        curve=SECP256R1(),
    )
    pub_key = pub_numbers.public_key()
    sec1_bytes = pub_key.public_bytes(Encoding.X962, PublicFormat.CompressedPoint)
    pub_key_b64 = base64.b64encode(sec1_bytes).decode()

    record = {
        "credential_id": cred_id_hex,
        "credential_type": "fido2-es256",
        "public_key": pub_key_b64,
        "label": args.label,
        "enrolled_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }
    print(json.dumps(record))


def cmd_assert(args):
    challenge_hex = args.challenge
    challenge_bytes = bytes.fromhex(challenge_hex)
    client_data_hash = hashlib.sha256(challenge_bytes).digest()

    dev = find_device()
    ctap = Ctap2(dev)

    print("touch the key when it blinks...", file=sys.stderr)

    pin_token, pin_proto = get_pin_token_if_needed(ctap)

    kwargs = {}
    if pin_token is not None:
        kwargs["pin_uv_param"] = pin_proto.authenticate(pin_token, client_data_hash)
        kwargs["pin_uv_protocol"] = pin_proto.VERSION

    assertion = ctap.get_assertion(
        rp_id=RP_ID,
        client_data_hash=client_data_hash,
        options={"up": True},
        **kwargs,
    )

    auth_data_bytes = bytes(assertion.auth_data)
    sig_bytes = assertion.signature
    cred_id = assertion.credential["id"] if assertion.credential else None

    result = {
        "credential_id": cred_id.hex() if cred_id else None,
        "credential_type": "fido2-es256",
        "signature": base64.b64encode(sig_bytes).decode(),
        "authenticator_data": base64.b64encode(auth_data_bytes).decode(),
    }
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="FIDO2 helper for gateway arm ceremony")
    sub = parser.add_subparsers(dest="command")

    enroll_p = sub.add_parser("enroll", help="Create resident credential")
    enroll_p.add_argument("--label", default="yubikey-fido2", help="Credential label")

    assert_p = sub.add_parser("assert", help="Get assertion for challenge")
    assert_p.add_argument("--challenge", required=True, help="Challenge bytes in hex")

    args = parser.parse_args()
    if args.command == "enroll":
        cmd_enroll(args)
    elif args.command == "assert":
        cmd_assert(args)
    else:
        parser.print_help()
        sys.exit(1)
