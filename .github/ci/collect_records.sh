#!/usr/bin/env bash
# 失败时把 $RUNNER_TEMP 下匹配的运行记录按相对路径复制到 $RUNNER_TEMP/failure-records，供只给一个路径的上传步骤使用。
# 容器 job 中 upload-artifact 的多行 path 只有第一行会换算成容器内路径，其余行仍指向宿主机而找不到文件，
# 且 if-no-files-found: ignore 不报错；因此不在上传步骤里列多个路径（同一写法也用于不在容器中的 job）。
# 用法：bash .github/ci/collect_records.sh <相对 $RUNNER_TEMP 的 glob>...
set -euo pipefail
shopt -s nullglob
dest="$RUNNER_TEMP/failure-records"
mkdir -p "$dest"
cd "$RUNNER_TEMP"
for pattern in "$@"; do
  # shellcheck disable=SC2086 # 有意按 glob 展开；不用数组，macOS 的 bash 3.2 在 set -u 下展开空数组会报错
  for item in $pattern; do
    # 不含通配符的名称不受 nullglob 影响，不存在时原样返回。
    [ -e "$item" ] || continue
    mkdir -p "$dest/$(dirname "$item")"
    cp -R "$item" "$dest/$item"
    echo "collected $item"
  done
done
