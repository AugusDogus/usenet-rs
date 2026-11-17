"""Writes the yEnc test articles in this directory.

No freely licensed real articles were at hand, so these are encoded here, independently of
the crate, the way common posters (Nyuu, ngPost) write them: 128-column lines that may run
one over to finish an escape, NUL, LF, CR and '=' always escaped, '.' escaped at a line
start, and TAB and SPACE escaped at either end of a line. Payloads are seeded pseudo-random
bytes, so the files are reproducible. Run: python3 generate.py
"""

import pathlib
import random
import zlib

HERE = pathlib.Path(__file__).parent


def encode(data: bytes) -> bytes:
    out = bytearray()
    col = 0
    for i, b in enumerate(data):
        c = (b + 42) & 0xFF
        edge = col == 0 or col >= 127 or i == len(data) - 1
        if c in (0, 10, 13, 61) or (edge and c in (9, 32)) or (col == 0 and c == 46):
            out += bytes((61, (c + 64) & 0xFF))
            col += 2
        else:
            out.append(c)
            col += 1
        if col >= 128:
            out += b"\r\n"
            col = 0
    if col:
        out += b"\r\n"
    return bytes(out)


def multipart(name, whole, part, total, begin, end, data, file_crc=False, declared_end=None):
    head = f"=ybegin part={part} total={total} line=128 size={len(whole)} name={name}\r\n"
    head += f"=ypart begin={begin + 1} end={declared_end or end}\r\n"
    tail = f"=yend size={len(data)} part={part} pcrc32={zlib.crc32(data):08x}"
    if file_crc:
        tail += f" crc32={zlib.crc32(whole):08x}"
    return head.encode() + encode(data) + (tail + "\r\n").encode()


def write(name, body):
    (HERE / name).write_bytes(body)


rng = random.Random(20261117)
rar = rng.randbytes(1_900_000)
write("multipart-first.yenc", multipart("sample.part01.rar", rar, 1, 3, 0, 768_000, rar[:768_000]))
write("multipart-last.yenc", multipart("sample.part01.rar", rar, 3, 3, 1_536_000, 1_900_000, rar[1_536_000:], True))

small = rng.randbytes(50_000)
# A last part whose `end` is wrong, as some posters write it.
write(
    "wrong-end.yenc",
    multipart("sample.part02.rar", small, 2, 2, 40_000, 50_000, small[40_000:], declared_end=10_000),
)

par2 = rng.randbytes(40_432)
write("single-part.yenc", multipart("sample.vol00+01.par2", par2, 1, 1, 0, len(par2), par2, True))

nfo = bytes(rng.choice(b"abcdefghijklmnopqrstuvwxyz .\t\r\n=") for _ in range(6_000))
body = f"=ybegin line=128 size={len(nfo)} name=sample.nfo\r\n".encode() + encode(nfo)
write("single.yenc", body + f"=yend size={len(nfo)} crc32={zlib.crc32(nfo):08x}\r\n".encode())

# Bytes that encode to characters a poster must escape, so escapes land at every column.
critical = bytes((c - 42) & 0xFF for c in (0, 10, 13, 61, 46, 9, 32))
stress = bytes(rng.choice(critical) if rng.random() < 0.5 else rng.randrange(256) for _ in range(30_000))
whole = rng.randbytes(10_000) + stress
write("escapes.yenc", multipart("sample.part03.rar", whole, 2, 2, 10_000, len(whole), stress))

for f in sorted(HERE.glob("*.yenc")):
    print(f.name, f.stat().st_size)
for name, data in [("rar", rar), ("small", small), ("par2", par2), ("nfo", nfo), ("whole", whole)]:
    print(name, len(data), f"{zlib.crc32(data):08x}")
print("first", f"{zlib.crc32(rar[:768_000]):08x}", "last", f"{zlib.crc32(rar[1_536_000:]):08x}")
print("wrong-end", f"{zlib.crc32(small[40_000:]):08x}", "stress", f"{zlib.crc32(stress):08x}")
