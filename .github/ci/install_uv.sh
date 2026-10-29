#!/usr/bin/env bash
# 用法：install_uv.sh <python>。把固定版本的 uv 装进该解释器的环境（发行、快速层与对照层共用）。
# 不经 PyPI 索引：按 requirements/uv-wheels.txt 的固定地址下载本平台 wheel 并核对 sha256（fetch.py），
# 再从本地文件安装；装好后核对版本等于 UV_VERSION。
set -euo pipefail
python=$1
wheel=$("$python" .github/ci/fetch.py uv --list .github/ci/requirements/uv-wheels.txt --version "$UV_VERSION" \
  --out-dir "${RUNNER_TEMP:?}/uv-wheel")
"$python" -m pip install --quiet --no-index --no-deps "$wheel"
installed=$("$python" -m uv --version)
case "$installed" in
  "uv $UV_VERSION" | "uv $UV_VERSION "*) ;;
  *) echo "installed $installed, expected uv $UV_VERSION" >&2; exit 1 ;;
esac
