#!/usr/bin/env python3
"""Set the FIDO2 PIN on a connected YubiKey.

Usage: set-fido2-pin.py <pin>
"""
import sys
from fido2.hid import CtapHidDevice
from fido2.ctap2 import Ctap2
from fido2.ctap2.pin import ClientPin, PinProtocolV2

if len(sys.argv) != 2 or len(sys.argv[1]) < 4:
    print("usage: set-fido2-pin.py <pin>  (min 4 chars)", file=sys.stderr)
    sys.exit(1)

dev = list(CtapHidDevice.list_devices())[0]
ctap = Ctap2(dev)
pin_client = ClientPin(ctap, PinProtocolV2())
pin_client.set_pin(sys.argv[1])
print("PIN set successfully")
