#!/usr/bin/env bash
# The static ort-sys Linux distribution references glibc 2.38 symbols. Use
# Microsoft's same-version manylinux_2_28 C library for the Python wheels.
set -euo pipefail

lib_dir=${1:?supply a temporary library directory}
# CPython 3.12 is only the carrier for the native library. The library itself
# has no libpython dependency and is shared across all Python ABI builds.
printf '%s\n' 'onnxruntime==1.31.0 --hash=sha256:35758d7606d578ec5b9d65f6e8a1f488013194c3f6097038a3223cb26d35ef9a --hash=sha256:5e129d6c56abd53e659cb70f00a108d6824086470ff99c2e47a82e5786563db3' |
  /opt/python/cp312-cp312/bin/python -m pip download --require-hashes --only-binary=:all: \
    --no-deps --dest "$lib_dir" -r /dev/stdin

wheel=("$lib_dir"/*.whl)
unzip -j "${wheel[0]}" 'onnxruntime/capi/libonnxruntime.so.1.31.0' -d "$lib_dir"
ln -s libonnxruntime.so.1.31.0 "$lib_dir/libonnxruntime.so.1"
ln -s libonnxruntime.so.1 "$lib_dir/libonnxruntime.so"
notices=pensyve-python/python/pensyve/licenses/onnxruntime
mkdir -p "$notices"
unzip -j "${wheel[0]}" onnxruntime/LICENSE onnxruntime/ThirdPartyNotices.txt -d "$notices"
