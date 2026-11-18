# PAR2 test set

Written by par2cmdline 1.4.0 (from nixpkgs) with `gen.sh`: three files of pseudo-random
bytes from fixed seeds, protected with 4096-byte slices and 8 recovery blocks in three
volumes (`par2 create -s4096 -c8 -n3`). The files hold no third-party content and are
covered by this crate's MIT license.

| File | What it is |
| --- | --- |
| `set.par2` | the index: the main, file description and slice checksum packets |
| `set.volA+B.par2` | volumes holding recovery blocks `A` to `A+B-1`, and copies of the index's packets |
| `a.bin`, `b.bin`, `c.bin` | the protected files: 44 slices, the last of each file short, and an odd size among them |
| `a.bin.damaged` | `a.bin` with bytes 8000 to 8767 zeroed, two slices' worth of damage that par2cmdline repairs |
| `SHA256SUMS` | the files' SHA-256 |
