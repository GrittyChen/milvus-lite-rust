#!/bin/bash
# 在仓库外的工作目录里为本仓的 Rust 实现建第三套环境：rs = milvus-lite-rust（由本仓构建的 wheel）。
# 用法: setup_rs.sh <work_dir> [pymilvus 版本，默认 3.0.2]
# 需要仓库开发环境中的 maturin（见 AGENTS.md「常用命令」）。rs 环境里不能装官方的 milvus-lite：两者占用同一个
# milvus_lite 模块。setup.sh 建的 v2 / v3 不依赖本脚本，复现 0001 不需要 Rust 工具链。
set -euo pipefail
WORK=${1:?用法: setup_rs.sh <work_dir> [pymilvus 版本]}
PYMILVUS=${2:-3.0.2}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
rm -rf "$WORK/wheels_rs"
"$REPO/.venv/bin/maturin" build --release -q -m "$REPO/crates/py/Cargo.toml" -o "$WORK/wheels_rs"
uv venv -q -p 3.12 --clear "$WORK/rs"
uv pip install -q -p "$WORK/rs/bin/python" "pymilvus==$PYMILVUS" numpy "$WORK"/wheels_rs/milvus_lite_rust-*.whl
# 包的版本号区分不了构建，记下构建所用的 commit，bench.py 把它写进结果、summarize.py 按它分组。
# 工作区有未提交改动时附上改动内容的指纹：同一 commit 上的不同版本才不会被汇总成一组
commit=$(git -C "$REPO" rev-parse HEAD)
# 构建身份只取引擎与打包输入，基准或本地工作日志不会改变 native 产物。
if [ -n "$(git -C "$REPO" status --porcelain -- Cargo.toml Cargo.lock pyproject.toml rust-toolchain.toml crates)" ]; then
  fp=$( (git -C "$REPO" diff HEAD -- Cargo.toml Cargo.lock pyproject.toml rust-toolchain.toml crates; git -C "$REPO" ls-files -o --exclude-standard -z -- Cargo.toml Cargo.lock pyproject.toml rust-toolchain.toml crates | (cd "$REPO" && xargs -0 cat)) | shasum -a 256 | cut -c1-8)
  commit="$commit+$fp"
fi
echo "$commit" > "$WORK/rs/BUILD_COMMIT"
"$WORK/rs/bin/python" -c "import importlib.metadata as m, pymilvus; print('rs ok, milvus-lite-rust', m.version('milvus-lite-rust'), 'pymilvus', pymilvus.__version__)"
