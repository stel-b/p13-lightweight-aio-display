# Extracts the static handshake data from a USB capture of MSI's driver.
# Run once on the dev machine (needs tshark on PATH or in the default Wireshark dir).
#
#   python scripts/extract_handshake.py [capture.pcapng] [out_dir]
#
# Defaults: research/coldstart.pcapng, %ProgramData%\aio-ui
# Never commit the output files.
import os, shutil, subprocess, sys
from pathlib import Path

PCAP = sys.argv[1] if len(sys.argv) > 1 else "research/coldstart.pcapng"
OUT = Path(sys.argv[2] if len(sys.argv) > 2 else Path(os.environ.get("ProgramData", r"C:\ProgramData")) / "aio-ui")
TSHARK = shutil.which("tshark") or r"C:\Program Files\Wireshark\tshark.exe"

def cap(frame):
    out = subprocess.run([TSHARK, "-r", PCAP, "-Y", f"frame.number == {frame}",
                          "-T", "fields", "-e", "usb.capdata"],
                         capture_output=True, text=True, check=True).stdout.strip()
    return bytes.fromhex(out.replace(":", ""))

blob1, final60 = cap(29627), cap(29687)
auth1, auth2 = cap(29682), cap(29686)
assert len(blob1) == 256 and len(final60) == 60, (len(blob1), len(final60))
assert len(auth1) == 202 and len(auth2) == 256, (len(auth1), len(auth2))

OUT.mkdir(parents=True, exist_ok=True)
(OUT / "handshake.bin").write_bytes(blob1 + final60)
(OUT / "expected_responses.bin").write_bytes(auth1 + auth2)
print(f"Wrote {OUT / 'handshake.bin'} (316 bytes) and {OUT / 'expected_responses.bin'} (458 bytes)")
