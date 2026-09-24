#!/bin/bash
# 在仓库外的工作目录里建两套隔离环境：v2 = milvus-lite 2.5.1（C++），v3 = milvus-lite 3.x（纯 Python）。
# 用法: setup.sh <work_dir> [v3 版本，默认 3.2.1] [pymilvus 版本，默认 3.0.2]
set -euo pipefail
WORK=${1:?用法: setup.sh <work_dir> [v3_version] [pymilvus_version]}
V3=${2:-3.2.1}
PYMILVUS=${3:-3.0.2}
mkdir -p "$WORK"
uv venv -q --clear -p 3.12 "$WORK/v2"
uv venv -q --clear -p 3.12 "$WORK/v3"
# 2.5.1 的 __init__ 依赖 pkg_resources，setuptools 81 起已移除
uv pip install -q -p "$WORK/v2/bin/python" "milvus-lite==2.5.1" "pymilvus==$PYMILVUS" numpy "setuptools<81"
uv pip install -q -p "$WORK/v3/bin/python" "milvus-lite==$V3" "pymilvus==$PYMILVUS" numpy
"$WORK/v2/bin/python" -W ignore -c "import pymilvus; print('v2 ok, pymilvus', pymilvus.__version__)"
"$WORK/v3/bin/python" -c "import milvus_lite, pymilvus; print('v3 ok, milvus-lite', milvus_lite.__version__, 'pymilvus', pymilvus.__version__)"
