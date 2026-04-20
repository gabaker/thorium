# Tool Description Tracking

Status of markdown descriptions for all Thorium toolbox images and pipelines.
Generated 2026-06-11 during the description verification pass (every description was rebuilt from authoritative source documentation; facts were not taken from prior content or model memory).

## Coverage Summary

| Scope | Total | description.md present | Missing |
|---|---|---|---|
| Images | 60 | 60 | 0 |
| Pipelines | 34 | 34 | 0 |

Every image and pipeline directory has a reviewed `description.md`. No documentation lookup failed outright — every tool's description is grounded in authoritative material (project docs, a faithful mirror, or the tool's own source code). Workarounds and gaps are listed below.

## Config JSON Status (`description` field)

COMPLETE (2026-06-11): every `description.md` has been injected into its config JSON `description` field via `json.dump` (proper escaping verified; all 94 configs re-parse and round-trip exactly against their markdown source).

| Status | Image configs | Pipeline configs |
|---|---|---|
| present | 60 | 34 |
| stub | 0 | 0 |
| missing | 0 | 0 |

## Documentation Access Notes

The build environment has a network egress allowlist; several canonical documentation hosts were unreachable. None caused a failure — each was resolved via a mirror, upstream source, or user action:

- **blog.didierstevens.com/tools/xorstrings** — blog.didierstevens.com initially blocked, unblocked on request and fetched; also grounded in in-repo XORStrings.c source
- **github.com/H3xKatana/autoVolatility3** — README fetched from GitHub. No license published upstream.
- **github.com/google/magika** — securityresearch.google blocked; GitHub README sufficed
- **github.com/mandiant/quantumstrand** — No README in binary release; used floss/qs readme.md + main.py from flare-floss quantumstrand branch (source pointer provided by user)
- **github.com/mbeijen/mimetype** — metacpan.org blocked and mbeijen repo 404; used File::MimeInfo script POD from michal-josef-spacek/File-MimeInfo GitHub
- **github.com/wireshark/tshark** — wireshark.org blocked; used tshark.adoc man page from wireshark/wireshark GitHub
- **gnu.org/binutils/dump-symbols** — sourceware.org blocked; used binutils.texi (nm section) from Distrotech/binutils GitHub mirror
- **gnu.org/binutils/strings** — sourceware.org blocked; used binutils.texi from Distrotech/binutils GitHub mirror
- **gnu.org/binutils/strings-16be** — sourceware.org blocked; used binutils.texi from Distrotech/binutils GitHub mirror
- **gnu.org/binutils/strings-16le** — sourceware.org blocked; used binutils.texi from Distrotech/binutils GitHub mirror
- **gnu.org/binutils/strings-32be** — sourceware.org blocked; used binutils.texi from Distrotech/binutils GitHub mirror
- **gnu.org/binutils/strings-32le** — sourceware.org blocked; used binutils.texi from Distrotech/binutils GitHub mirror
- **mark0.net/software/trid** — mark0.net initially blocked, unblocked on request and fetched. No license published upstream.
- **salsa.debian.org/pkg-security-team/fcrackzip** — salsa.debian.org blocked; used README+COPYING from hyc/fcrackzip GitHub mirror
- **salsa.debian.org/rul/foremost** — salsa.debian.org blocked; used README from korczis/foremost GitHub mirror. No license published upstream.
- **sqlite.org/sqlite/sqlitediff** — sqlite.org blocked; used sqldiff.c header docs from sqlite/sqlite GitHub source
- **sqlite.org/sqlite/sqlitedump** — sqlite.org blocked; used shell.c.in (.dump/.dbinfo help text) from sqlite/sqlite GitHub source

Tools with no published license anywhere upstream (license correctly recorded as unstated): **TrID**, **foremost**, **autoVolatility3**.

## Per-Image Status

| Image | description.md | JSON desc | License (verified) |
|---|---|---|---|
| blog.didierstevens.com/tools/xorstrings | yes | missing | Public Domain |
| exiftool.org/tools/exiftool | yes | missing | Artistic-1.0-Perl OR GPL-1.0-or-later (released under the... |
| github.com/Cisco-Talos/ClamAV | yes | missing | GPL-2.0 |
| github.com/H3xKatana/autoVolatility3 | yes | stub | unstated |
| github.com/OSIF/suricata | yes | missing | unstated (docs do not name a license; project is widely G... |
| github.com/ReFirmLabs/binwalk | yes | present | MIT |
| github.com/aquasecurity/trivy | yes | stub | Apache-2.0 |
| github.com/cmu-sei/pharos-apianalyzer | yes | missing | BSD-3-Clause |
| github.com/cmu-sei/pharos-callanalyzer | yes | missing | BSD |
| github.com/cmu-sei/pharos-fn2hash | yes | missing | BSD (Carnegie Mellon University / SEI) |
| github.com/cmu-sei/pharos-ooanalyzer | yes | missing | BSD |
| github.com/decalage2/balbuzard | yes | present | BSD-2-Clause |
| github.com/erocarrera/pefile | yes | missing | unstated |
| github.com/file/file | yes | missing | Distributable (BSD-style) — README states "copyright but ... |
| github.com/fkie-cad/cwe-checker | yes | missing | LGPL-3.0-or-later |
| github.com/gamelinux/prads | yes | missing | GPL-2.0-or-later |
| github.com/google/magika | yes | missing | Apache-2.0 |
| github.com/hellman/xortool | yes | missing | MIT |
| github.com/horsicq/detect-it-easy | yes | missing | unstated |
| github.com/intel/cve-bin-tool/cve | yes | present | unstated (not named in the authoritative README; project ... |
| github.com/intel/cve-bin-tool/sbom | yes | present | GPL (project is GPL-licensed per source; the authoritativ... |
| github.com/llnl/surfactant | yes | stub | MIT |
| github.com/mandiant/capa | yes | missing | Apache-2.0 |
| github.com/mandiant/flare-floss | yes | missing | Apache-2.0 |
| github.com/mandiant/quantumstrand | yes | missing | unstated |
| github.com/mandiant/stringsifter | yes | missing | unstated |
| github.com/mbeijen/mimetype | yes | missing | Same terms as Perl (Artistic-1.0-Perl OR GPL-1.0-or-later) |
| github.com/onekey-sec/unblob | yes | missing | MIT |
| github.com/owasp-dep-scan/blint | yes | present | unstated |
| github.com/packing-box/peid | yes | missing | unstated (README shows a PyPI License badge but does not ... |
| github.com/pdfminer/pdf2text | yes | stub | unstated |
| github.com/ralphje/signify | yes | missing | unstated |
| github.com/rocky/python-uncompyle6 | yes | missing | unstated (authoritative doc names no license) |
| github.com/simsong/bulk_extractor | yes | present | unstated in fetched docs (README links to LICENSE.md; pre... |
| github.com/simsong/tcpflow | yes | missing | unstated |
| github.com/ssdeep-project/ssdeep | yes | missing | unstated (not named in the authoritative API doc) |
| github.com/theopolis/uefi-firmware-parser | yes | stub | MIT |
| github.com/trailofbits/polyfile | yes | missing | Apache-2.0 |
| github.com/trufflesecurity/trufflehog | yes | stub | AGPL-3.0 |
| github.com/upx/upx | yes | stub | GPL-2.0-or-later (GPLv2+, or at your option GPLv2+ with s... |
| github.com/virustotal/yara | yes | stub | unstated |
| github.com/wireshark/tshark | yes | missing | unstated (provided documentation does not name a license) |
| github.com/zeek/zeek | yes | missing | BSD (BSD-3-Clause) |
| gnu.org/binutils/dump-symbols | yes | present | GPL (GNU Binutils; specific version not named in the prov... |
| gnu.org/binutils/strings | yes | present | GPL (GNU General Public License) — not named in the provi... |
| gnu.org/binutils/strings-16be | yes | stub | GPL (GNU Binutils; source excerpt does not state an expli... |
| gnu.org/binutils/strings-16le | yes | stub | GPL (GNU General Public License) — version not stated in ... |
| gnu.org/binutils/strings-32be | yes | stub | unstated (GNU Binutils is distributed under the GPL, but ... |
| gnu.org/binutils/strings-32le | yes | stub | unstated (GNU Binutils; documentation excerpt does not na... |
| mark0.net/software/trid | yes | missing | unstated |
| salsa.debian.org/pkg-security-team/fcrackzip | yes | missing | unstated |
| salsa.debian.org/rul/foremost | yes | missing | unstated |
| sandia.gov/auto-volatility3/generator | yes | present | Thorium license (see repo LICENSE) |
| sandia.gov/auto-volatility3/worker | yes | stub | Thorium license (Apache-2.0, per Thorium LICENSE) |
| sandia.gov/byte-frequency | yes | present | Thorium license (project LICENSE) |
| sandia.gov/emails/email-parser | yes | stub | Thorium license (see repo LICENSE) |
| sandia.gov/pdfs/PdfMiner | yes | missing | Thorium license (see repo LICENSE) |
| sandia.gov/pdfs/PdfPreview | yes | stub | Thorium license (see repo LICENSE) |
| sqlite.org/sqlite/sqlitediff | yes | stub | Public Domain |
| sqlite.org/sqlite/sqlitedump | yes | stub | Public Domain |

## Per-Pipeline Status

| Pipeline | description.md | JSON desc |
|---|---|---|
| antivirus | yes | missing |
| auto-volatility3/generator | yes | missing |
| auto-volatility3/worker | yes | missing |
| autovolatility3 | yes | stub |
| balbuzard | yes | present |
| binwalk | yes | present |
| blint | yes | present |
| bulk-extractor | yes | present |
| bytefrequency | yes | present |
| capa | yes | missing |
| cve-bin-tool | yes | present |
| cwe-checker | yes | missing |
| dump-symbols | yes | missing |
| email-parser | yes | stub |
| foremost | yes | missing |
| identify-files | yes | missing |
| pcap-analyze | yes | missing |
| pdf-analyze | yes | stub |
| pefile | yes | missing |
| pharos | yes | missing |
| python-uncompyle6 | yes | missing |
| signify | yes | missing |
| sqlitediff | yes | missing |
| sqlitedump | yes | missing |
| ssdeep | yes | missing |
| strings | yes | missing |
| surfactant | yes | stub |
| trivy | yes | stub |
| trufflehog | yes | missing |
| uefi-extract | yes | missing |
| unblob | yes | missing |
| unzip | yes | missing |
| upx-unpack | yes | stub |
| yara | yes | missing |

## Config Anomalies Found During Verification

- `tools/pipelines/static/sqlitediff/manifest.toml` references `[images.sqlitediff]` but the image config registers `name = "sqldiff"` — name mismatch may break toolbox resolution.
- `tools/images/github.com/H3xKatana/autoVolatility3/` has no `manifest.toml` (only Dockerfile + config JSON).
- `tools/pipelines/static/autovolatility3/` and `tools/pipelines/static/cwe-checker/` have no `manifest.toml`.
- `tools/images/github.com/theopolis/uefi-firmware-parser` and `github.com/trufflesecurity/trufflehog` config JSONs had the literal string "null" as description (counted as stub).

## Next Steps

1. ~~Manual review of all `description.md` files.~~ (done)
2. ~~Inject each `description.md` into its config JSON `description` field.~~ (done 2026-06-11)
3. Rebuild the toolbox via the new thorctl toolbox functionality.
4. Resolve the `sqlitediff` pipeline / `sqldiff` image name mismatch before the rebuild.
