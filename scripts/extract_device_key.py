# Extracts the P13's RSA public key from MSI's display driver, so the daemon
# can compute the handshake (see docs/handshake.md). Run once.
#
#   python scripts/extract_device_key.py [AicUsbDisplayDriver.dll] [out_dir]
#
# Default driver: the installed one in the Windows driver store, else the copy
# in research/msi/bin/driver. Default output: %ProgramData%\aio-ui.
# The key is not secret, but it comes from MSI's driver: don't commit it.
import base64, glob, os, re, sys
from pathlib import Path

STORE = r"C:\Windows\System32\DriverStore\FileRepository\aicusbdisplaydriver.inf_*\AicUsbDisplayDriver.dll"
COPY = Path(__file__).resolve().parent.parent / "research" / "msi" / "bin" / "driver" / "AicUsbDisplayDriver.dll"

def find_driver():
    found = sorted(glob.glob(STORE))
    if found:
        return Path(found[-1])
    if COPY.is_file():
        return COPY
    sys.exit("AicUsbDisplayDriver.dll not found; pass its path as the first argument")

def rsa_modulus_bits(der):
    """Bit length of the modulus in a SubjectPublicKeyInfo, as a sanity check."""
    def tlv(b, i):
        tag, ln, i = b[i], b[i + 1], i + 2
        if ln & 0x80:
            n = ln & 0x7F
            ln, i = int.from_bytes(b[i:i + n], "big"), i + n
        return b[i:i + ln], i + ln
    spki, _ = tlv(der, 0)
    _, j = tlv(spki, 0)              # algorithm identifier
    bits, _ = tlv(spki, j)           # BIT STRING
    rsakey, _ = tlv(bits[1:], 0)
    modulus, _ = tlv(rsakey, 0)
    return int.from_bytes(modulus, "big").bit_length()

driver = Path(sys.argv[1]) if len(sys.argv) > 1 else find_driver()
out_dir = Path(sys.argv[2] if len(sys.argv) > 2 else Path(os.environ.get("ProgramData", r"C:\ProgramData")) / "aio-ui")

data = driver.read_bytes()
blocks = re.findall(rb"-----BEGIN PUBLIC KEY-----[A-Za-z0-9+/=\r\n]+-----END PUBLIC KEY-----", data)
if len(blocks) != 1:
    sys.exit(f"expected exactly one PUBLIC KEY in {driver}, found {len(blocks)}")
pem = blocks[0].replace(b"\r\n", b"\n").decode() + "\n"
body = "".join(pem.splitlines()[1:-1])
bits = rsa_modulus_bits(base64.b64decode(body))
if bits != 2048:
    sys.exit(f"unexpected key size {bits} bits")

out_dir.mkdir(parents=True, exist_ok=True)
(out_dir / "device_key.pem").write_text(pem)
print(f"Wrote {out_dir / 'device_key.pem'} (RSA-{bits}) from {driver}")
