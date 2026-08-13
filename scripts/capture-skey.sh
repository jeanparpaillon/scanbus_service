#!/usr/bin/env bash
# Capture the vendor daemon doing what scanbus reimplements: the SNMP registration on
# UDP/161 and the panel event on UDP/54925.
#
# This is the one step of issue 5.7 that cannot be done from a disassembly. It needs
# CAP_NET_RAW (hence sudo) and someone standing at the printer, because the interesting
# half only happens when a panel entry is chosen. Everything else about the protocol was
# read out of brscan-skey-exe, which ships unstripped — see
# scanbus-backend-brother/src/skey/mod.rs.
#
# Usage:
#   scripts/capture-skey.sh [output.pcap]
#
# Then, at the printer, for each of the four entries under Scan > to PC:
#   Image, OCR, E-mail, File
# choose this host and press Start. One press per entry, in that order.
#
# Finally: cp <output.pcap> scanbus-backend-brother/tests/fixtures/skey.pcap
#
# Note that the registration string carries the login name of whoever ran brscan-skey and
# this host's IP address. Read the capture before committing it.

set -euo pipefail

out=${1:-skey.pcap}
skey=/opt/brother/scanner/brscan-skey/brscan-skey

if [[ ! -x $skey ]]; then
    echo "brscan-skey is not installed at $skey — nothing to capture." >&2
    echo "It is a .deb from https://support.brother.com/ and is in no apt repository." >&2
    exit 1
fi

if ! command -v tcpdump >/dev/null; then
    echo "tcpdump is not installed (apt install tcpdump)." >&2
    exit 1
fi

# A running daemon already holds UDP/54925, so a second one registers nothing and the
# capture comes out with the 161 half only.
if pgrep -x brscan-skey-exe >/dev/null; then
    echo "brscan-skey is already running; stop it first:" >&2
    echo "    $skey -t" >&2
    exit 1
fi

echo "Capturing to $out — ^C when the four presses are done."
sudo tcpdump -i any -s0 -w "$out" 'udp port 161 or udp port 54925' &
tcpdump_pid=$!
# Give tcpdump time to open the socket, or the registration goes out before the filter is
# in place and the capture is missing the only packet that is hard to reproduce.
sleep 2

"$skey"

cat <<'EOF'

brscan-skey is registered. At the printer, under Scan > to PC, choose this host under
each of Image, OCR, E-mail and File in turn and press Start. Then come back and press ^C.

EOF

trap 'kill "$tcpdump_pid" 2>/dev/null || true; "$skey" -t 2>/dev/null || true' EXIT
wait "$tcpdump_pid" || true

sudo chown "$(id -u):$(id -g)" "$out"
echo "Wrote $out"
