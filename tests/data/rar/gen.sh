#!/usr/bin/env bash
# Regenerates the archives here with RAR 6.24 (`RAR=/path/to/rar ./gen.sh`). The files
# inside are pseudo-random bytes and repeated text made by this script, so the archives
# carry no third-party content. Each case is a folder of volumes named as rar names them.
set -euo pipefail
RAR=${RAR:?set RAR to a rar 6.x binary}
OUT=$(cd "$(dirname "$0")" && pwd)
SRC=$(mktemp -d "${TMPDIR:-$HOME/.cache}/rar-src.XXXXXX")
trap 'rm -rf "$SRC"' EXIT
bytes() { python3 -c "import random,sys; r=random.Random('$2'); sys.stdout.buffer.write(r.randbytes($1))"; }
cd "$SRC"
mkdir -p single multi/Sample multi/Subs vols late text
bytes 5000 single > single/file.bin
bytes 8000 movie > multi/Movie.2024.mkv
bytes 2000 sample > multi/Sample/sample.mkv
bytes 500 subs > "multi/Subs/Ünïcödé 日本語.srt"
bytes 6000 sample > vols/sample.mkv
bytes 40000 movie > vols/Movie.2024.mkv
bytes 300 nfo > vols/movie.nfo
bytes 20000 big > late/1_big.bin
bytes 3000 small > late/2_small.mkv
for i in $(seq 1 1500); do echo "line $i of some very compressible text"; done > text/a.txt
echo "a comment" > comment.txt
mk() { # format case source-dir switches... [-- files in archive order]
  local fmt=$1 name=$2 dir=$3; shift 3
  rm -rf "${OUT:?}/$fmt/$name"; mkdir -p "$OUT/$fmt/$name"
  local files=(.)
  local sw=()
  while [ $# -gt 0 ]; do
    if [ "$1" = -- ]; then shift; files=("$@"); break; fi
    sw+=("$1"); shift
  done
  (cd "$SRC/$dir" && "$RAR" a -idq -r -ma"${fmt#rar}" "${sw[@]}" "$OUT/$fmt/$name/arc.rar" "${files[@]}" >/dev/null)
}
for fmt in rar4 rar5; do
  mk $fmt single single -m0
  mk $fmt multi_dirs_unicode multi -m0
  mk $fmt volumes_new vols -m0 -v16k -- movie.nfo Movie.2024.mkv sample.mkv
  mk $fmt compressed text -m3
  mk $fmt enc_data vols -m0 -v16k -pSECRET -- movie.nfo Movie.2024.mkv sample.mkv
  mk $fmt enc_headers vols -m0 -v16k -hpSECRET -- movie.nfo Movie.2024.mkv sample.mkv
  mk $fmt late_header late -m0 -- 1_big.bin 2_small.mkv
  mk $fmt comment_rr vols -m0 -v16k -rr5% -z"$SRC/comment.txt" -- movie.nfo Movie.2024.mkv sample.mkv
done
mk rar4 volumes_old vols -m0 -v16k -vn -- movie.nfo Movie.2024.mkv sample.mkv
mk rar5 quickopen vols -m0 -v16k -qo+ -- movie.nfo Movie.2024.mkv sample.mkv
