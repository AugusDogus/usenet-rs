#!/usr/bin/env bash
# Regenerates the recovery set here with par2cmdline (`nix shell nixpkgs#par2cmdline -c ./gen.sh`).
# The files are pseudo-random bytes made from fixed seeds, so the set carries no
# third-party content. `a.bin.damaged` is `a.bin` with 768 bytes zeroed across two slices,
# as an article no server has leaves it; par2cmdline repairs it back to `a.bin`.
set -euo pipefail
cd "$(dirname "$0")"
rm -rf set && mkdir set && cd set
bytes() { python3 -c "import random,sys; r=random.Random('$2'); sys.stdout.buffer.write(r.randbytes($1))"; }
bytes 100000 a > a.bin
bytes 37 b > b.bin
bytes 70001 c > c.bin
par2 create -q -q -s4096 -c8 -n3 set.par2 a.bin b.bin c.bin
python3 -c "
d = bytearray(open('a.bin', 'rb').read())
d[8000:8768] = bytes(768)
open('a.bin.damaged', 'wb').write(d)"
sha256sum a.bin b.bin c.bin a.bin.damaged > SHA256SUMS
# par2cmdline itself must repair the damage, or the fixture proves nothing.
check=$(mktemp -d "${TMPDIR:-$HOME/.cache}/par2-check.XXXXXX")
trap 'rm -rf "$check"' EXIT
cp ./*.par2 b.bin c.bin "$check"
cp a.bin.damaged "$check/a.bin"
(cd "$check" && par2 repair -q -q set.par2 && sha256sum a.bin) | cut -d' ' -f1 | grep -qx "$(sha256sum a.bin | cut -d' ' -f1)"
