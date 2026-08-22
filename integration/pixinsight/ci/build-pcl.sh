#!/usr/bin/env bash
# Build libPCL-pxi.a from the pinned open-source PCL, out-of-tree, no PixInsight.
# Usage: build-pcl.sh --out <prefix-dir> [--work <clone-dir>]
#
# Adapted from MergeMosaic's integration/pixinsight/ci/build-pcl.sh (this
# file has no mmm/mpp-specific naming -- it only builds PCL itself -- so the
# adaptation is the pin values in pcl-pin.env, not this script's body).
#
# Divergence from MergeMosaic's *current* build-pcl.sh (deliberate): that
# version adds a HARD gate asserting the pinned tree's compiled
# PCL_API_Version equals a PCL_API_VERSION_HEX value in pcl-pin.env. mpp's
# pcl-pin.env intentionally omits that field (not independently verified
# against this SHA's actual header without a network fetch of the pinned
# commit, which this task didn't run) -- a wrong hard-coded value would hard
# **fail every CI run**. This script keeps only the softer Version.cpp
# major/minor/release check (warn, don't fail) that the *original*
# MergeMosaic build-pcl.sh (pre-API-gate) used; SHA pinning is still the
# authoritative, hard-gated identity check below.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$HERE/pcl-pin.env"

OUT="" ; WORK=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out)  OUT="$2"; shift 2 ;;
    --work) WORK="$2"; shift 2 ;;
    --help) echo "usage: build-pcl.sh --out <dir> [--work <dir>]"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ -n "$OUT" ] || { echo "--out is required" >&2; exit 2; }
WORK="${WORK:-$(mktemp -d)}"
mkdir -p "$OUT/lib" "$OUT/include" "$WORK"

# Fetch exactly the pinned commit (GitLab allows fetch-by-SHA for reachable commits).
cd "$WORK"
if [ ! -d .git ]; then
  git init -q
  git remote add origin "$PCL_REPO_URL"
fi
git fetch -q --depth 1 origin "$PCL_SHA"
git checkout -q FETCH_HEAD
HEAD_SHA="$(git rev-parse HEAD)"
[ "$HEAD_SHA" = "$PCL_SHA" ] || { echo "PCL SHA mismatch: got $HEAD_SHA want $PCL_SHA" >&2; exit 1; }

# Soft version check against Version.cpp (belt-and-suspenders; SHA is the hard gate).
VER_CPP="src/pcl/Version.cpp"
if [ -f "$VER_CPP" ]; then
  check_ver_fn() {
    # $1=Major|Minor|Release accessor name, $2=expected int value.
    # Anchored to "int Version::<fn>()" (not "PixInsightVersion::<fn>()") so it
    # only matches the PCL library version, not the confidential API version.
    grep -A2 "^int Version::$1()" "$VER_CPP" | grep -qE "return[[:space:]]+$2[[:space:]]*;"
  }
  check_ver_fn Major   "$PCL_VER_MAJOR"   || echo "warning: Version.cpp major != $PCL_VER_MAJOR (source may have shifted)" >&2
  check_ver_fn Minor   "$PCL_VER_MINOR"   || echo "warning: Version.cpp minor != $PCL_VER_MINOR (source may have shifted)" >&2
  check_ver_fn Release "$PCL_VER_RELEASE" || echo "warning: Version.cpp release != $PCL_VER_RELEASE (source may have shifted)" >&2
fi

# Build only the static PCL library (the mpp module links -lPCL-pxi and
# nothing else on Linux -- see module/makefile-x64's Linux PLATFORM_LIBS,
# which is just -lPCL-pxi -lpthread -lrt; unlike macOS it needs no 3rdparty
# archives). CUDADevice.cpp compiles without the CUDA toolkit in this PCL
# version. If a future pin breaks here on cuda.h, drop that TU from
# SRC_FILES or `apt-get install nvidia-cuda-toolkit` for headers.
export PCLDIR="$WORK"
export PCLSRCDIR="$WORK/src"
export PCLINCDIR="$WORK/include"
export PCLLIBDIR64="$WORK/lib/linux/x64"
export PCLBINDIR64="$WORK/bin"
mkdir -p "$PCLLIBDIR64" "$PCLBINDIR64"
( cd src/pcl/linux/g++ && make -f makefile-x64 -j"$(nproc)" )

# Locate and publish the archive + headers to the output prefix.
LIB="$(find "$WORK/src/pcl" -name libPCL-pxi.a -print -quit)"
[ -n "$LIB" ] || { echo "libPCL-pxi.a not produced" >&2; exit 1; }
cp -f "$LIB" "$OUT/lib/libPCL-pxi.a"
cp -a "$WORK/include/." "$OUT/include/"
echo "PCL built: $OUT/lib/libPCL-pxi.a ($(du -h "$OUT/lib/libPCL-pxi.a" | cut -f1)), headers in $OUT/include"
