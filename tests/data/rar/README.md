# RAR test archives

Written by RAR 6.24 (`rar a -ma4` and `-ma5`) with `gen.sh`, one folder per case of the
format matrix. The files inside are pseudo-random bytes and repeated text that `gen.sh`
makes from fixed seeds, so the archives hold no third-party content and are covered by
this crate's MIT license.

| Case | Switches | What it covers |
| --- | --- | --- |
| `single` | `-m0` | one stored file |
| `multi_dirs_unicode` | `-m0` | folders and a non-ASCII name |
| `volumes_new` | `-m0 -v16k` | `arc.partN.rar` volumes, a file split across all of them |
| `volumes_old` (RAR4) | `-m0 -v16k -vn` | `arc.rar`, `arc.r00`, ... volumes |
| `late_header` | `-m0` | a file whose header lies past the first article |
| `comment_rr` | `-m0 -v16k -rr5% -z` | an archive comment and recovery records |
| `quickopen` (RAR5) | `-m0 -v16k -qo+` | quick-open service headers |
| `compressed` | `-m3` | compressed data |
| `enc_data` | `-m0 -v16k -pSECRET` | encrypted file data, readable headers |
| `enc_headers` | `-m0 -v16k -hpSECRET` | encrypted headers |
