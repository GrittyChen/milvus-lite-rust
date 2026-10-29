#!/usr/bin/env bash
# 发行工作流各 job 的公共环境：CPython 3.10–3.15、固定版本的 uv、按 dev.txt 只用 wheel 安装的工具环境，
# 以及可选的 Rust 工具链（按 rust-toolchain.toml）。manylinux 容器内用镜像自带的 /opt/python，
# macOS 用 setup-python 装好的解释器。结果写入 GITHUB_ENV / GITHUB_PATH，供后续步骤使用。
# RELEASE_PYTHONS 列出本 job 要用的次版本号（如 "10 12"），只校验并导出这些；不设时为 3.10–3.15 全部。
set -euo pipefail
want_rust=${1:-}
read -r -a minors <<< "${RELEASE_PYTHONS:-10 11 12 13 14 15}"
for v in "${minors[@]}"; do
  if [ -x "/opt/python/cp3$v-cp3$v/bin/python" ]; then
    py="/opt/python/cp3$v-cp3$v/bin/python"
  else
    py=$(command -v "python3.$v")
  fi
  # 正式全套只针对 GIL 版 CPython；free-threaded 解释器只用于 sdist 的拒绝检查。
  "$py" -c "import sys; assert sys.version_info[:2] == (3, $v) and getattr(sys, '_is_gil_enabled', lambda: True)()"
  echo "PY3$v=$py" >> "$GITHUB_ENV"
  export "PY3$v=$py"
done
"$PY312" -m venv "$RUNNER_TEMP/uv-tool"
"$RUNNER_TEMP/uv-tool/bin/python" -m pip install --quiet "uv==$UV_VERSION"
echo "$RUNNER_TEMP/uv-tool/bin" >> "$GITHUB_PATH"
uv="$RUNNER_TEMP/uv-tool/bin/uv"
"$uv" venv -q -p "$PY312" "$RUNNER_TEMP/dev"
"$uv" pip install -q --python "$RUNNER_TEMP/dev/bin/python" --no-build -r .github/ci/requirements/dev.txt
echo "DEV_PY=$RUNNER_TEMP/dev/bin/python" >> "$GITHUB_ENV"
if [ "$want_rust" = rust ]; then
  if ! command -v rustup >/dev/null; then
    curl -sSf --retry 3 https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none
    echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  rustup toolchain install
  # 仓库外的构建（解包后的 sdist、orjson）读不到 rust-toolchain.toml，且以 --default-toolchain none 安装时
  # 没有默认工具链；之后显式安装 MSRV 还会把它变成默认。固定导出 RUSTUP_TOOLCHAIN，使所有 cargo 调用用同一版本，
  # 只有显式的 `cargo +<版本>`（MSRV 检查）例外。
  channel=$(sed -n 's/^channel *= *"\(.*\)"$/\1/p' rust-toolchain.toml)
  test -n "$channel"
  echo "RUSTUP_TOOLCHAIN=$channel" >> "$GITHUB_ENV"
  (cd / && RUSTUP_TOOLCHAIN="$channel" rustc --version && RUSTUP_TOOLCHAIN="$channel" cargo --version)
fi
