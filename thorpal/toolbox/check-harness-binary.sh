#!/usr/bin/env bash
# Check that a thorpal-harness binary runs on the harness image's runtime
#
#   ./check-harness-binary.sh <BINARY> [MAX_GLIBC]
#
# harness/Dockerfile packages a prebuilt binary into an Ubuntu 22.04 runtime, so the binary must
# need no glibc symbol version newer than 22.04's (2.35, or MAX_GLIBC) and must link OpenSSL 3
# (libssl.so.3 and libcrypto.so.3), or no OpenSSL at all. CI builds the binary on a build image
# that is meant to be the same Ubuntu release; this check makes a build on any other release fail
# loudly instead of publishing an image whose binary can't start. It prints the glibc versions the
# binary needs and its shared libraries, then exits 1 when either check fails, or when the file
# isn't a dynamically linked glibc ELF executable that objdump and ldd can read. It exits 2 when the
# binary or a tool it uses (objdump from binutils, ldd, od) is missing.
set -euo pipefail

# print text indented by two spaces, without its own leading whitespace
#
# Arguments:
#   $1 - the text
indent() {
  local line
  while read -r line; do
    printf '  %s\n' "$line"
  done <<< "$1"
}

# the binary to check and the newest glibc symbol version it may need
binary=${1:?usage: check-harness-binary.sh <BINARY> [MAX_GLIBC]}
max_glibc=${2:-2.35}
if [ ! -f "$binary" ]; then
  echo "no binary at $binary" >&2
  exit 2
fi
# without these tools the checks below would find nothing and pass
for tool in objdump ldd od; do
  if ! command -v "$tool" > /dev/null; then
    echo "ERROR: $tool is missing; install binutils (objdump), libc-bin (ldd), and coreutils (od)" >&2
    exit 2
  fi
done
# the file must be an ELF executable or position-independent executable (e_type 2 or 3, read as
# little-endian since the harness targets x86_64 and aarch64); od fails on a file shorter than the
# header, which leaves the fields empty
magic=$(od -An -tx1 -N4 "$binary" 2> /dev/null | tr -d ' \n' || true)
elf_type=$(od -An -tx1 -j16 -N2 "$binary" 2> /dev/null | tr -d ' \n' || true)
if [ "$magic" != "7f454c46" ] || { [ "$elf_type" != "0200" ] && [ "$elf_type" != "0300" ]; }; then
  echo "ERROR: $binary is not an ELF executable" >&2
  exit 1
fi
status=0
# the glibc symbol versions the binary's dynamic symbols need, oldest first; objdump exits 1 for a
# static, truncated, or unreadable file alike, so an empty list is the failure that counts
symbols=$(objdump -T "$binary" 2>&1) || echo "WARNING: objdump -T $binary failed: $(grep -m 1 . <<< "$symbols" || true)" >&2
glibc_versions=$(grep -o 'GLIBC_[0-9][0-9.]*' <<< "$symbols" | sed 's/^GLIBC_//' | sort -u -V || true)
echo "glibc symbol versions $binary needs:"
indent "${glibc_versions:-(none)}"
newest=$(tail -n 1 <<< "$glibc_versions")
if [ -z "$newest" ]; then
  echo "ERROR: no GLIBC_ symbol versions in $binary; it is not a dynamically linked glibc binary, or objdump could not read it" >&2
  status=1
elif [ "$(printf '%s\n%s\n' "$newest" "$max_glibc" | sort -V | tail -n 1)" != "$max_glibc" ]; then
  echo "ERROR: $binary needs GLIBC_$newest, newer than the harness runtime's $max_glibc; build it on Ubuntu 22.04" >&2
  status=1
fi
# the shared libraries the binary loads on this host; ldd fails for a file the dynamic loader
# can't load, which would not start on the runtime either
if ! libraries=$(ldd "$binary" 2>&1); then
  echo "ERROR: ldd could not list the shared libraries of $binary" >&2
  status=1
fi
echo "shared libraries $binary links:"
indent "$libraries"
# every OpenSSL library it links must be OpenSSL 3's, which the runtime's libssl3 package provides
while read -r lib _; do
  case "$lib" in
    libssl.so.3 | libcrypto.so.3) ;;
    libssl.so* | libcrypto.so*)
      echo "ERROR: $binary links $lib; the harness runtime has only libssl.so.3 and libcrypto.so.3" >&2
      status=1
      ;;
  esac
done <<< "$libraries"
# a library this host can't find would fail on the runtime too
if grep -q "not found" <<< "$libraries"; then
  echo "ERROR: $binary links a library this host can't find" >&2
  status=1
fi
if [ "$status" = 0 ]; then
  echo "$binary runs on the harness runtime (glibc <= $max_glibc, OpenSSL 3)"
fi
exit "$status"
