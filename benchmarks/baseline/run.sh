#!/bin/bash
# 顺序执行一组配置的各阶段（默认 build + reopen），结果逐行追加到 <work_dir>/<out>，每个阶段的 stderr 写入 <work_dir>/logs/。
# 用法: run.sh <work_dir> <N> <dim> <out.jsonl> <配置>...
#   配置形如 v2:FLAT:inproc、v3:HNSW:server（环境:索引:模式），依次执行 build 与 reopen；
#   加第 4 段 m1 时只执行 m1 阶段，如 rs:FLAT:inproc:m1
# 必须顺序跑：并行会互相抢 CPU，延迟数据失真。单个配置失败时记录并继续，最后以非零码退出。
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(cd "${1:?用法: run.sh <work_dir> <N> <dim> <out.jsonl> <配置>...}" && pwd) || exit 1
N=${2:?}; DIM=${3:?}; OUT=${4:?}; shift 4
mkdir -p "$WORK/logs"
failed=()
for cfg in "$@"; do
  IFS=: read -r env idx mode phases <<< "$cfg"
  case "${phases:-}" in
    "") phases="build reopen" ;;
    m1) ;;
    *) echo "!! $cfg: 第 4 段只能是 m1" >&2; failed+=("$cfg"); continue ;;
  esac
  py="$WORK/$env/bin/python"
  if [[ ! -x "$py" ]]; then
    echo "!! $cfg: 找不到 ${py}（先运行 setup.sh）" >&2; failed+=("$cfg"); continue
  fi
  # m1 的库放在单独的路径：不删掉 build 产生、供 concurrency.py 复用的库
  suffix=""; [[ "$phases" == m1 ]] && suffix="_m1"
  db="$WORK/r_${env}_${idx}_${mode}_${N}_${DIM}${suffix}.db"
  rm -rf "$db"
  echo "== $cfg" >&2
  for phase in $phases; do
    log="$WORK/logs/${env}_${idx}_${mode}_${N}_${DIM}_${phase}.log"
    if line=$("$py" "$HERE/bench.py" "$db" "$N" "$DIM" "$idx" "$phase" "$mode" 2>"$log" | grep '^{'); then
      echo "$line" >> "$WORK/$OUT"
    else
      echo "!! $cfg $phase 失败，见 $log" >&2; failed+=("$cfg:$phase"); break
    fi
  done
done
if (( ${#failed[@]} )); then
  echo "失败: ${failed[*]}" >&2; exit 1
fi
echo "done -> $WORK/$OUT" >&2
