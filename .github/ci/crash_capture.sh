# 在测试步骤开头以 source 执行（ulimit 只对当前 shell 及其子进程生效）。失败后由 crash_report.py 打出原生调用栈。
# Linux：崩溃的进程把 core 写到 $RUNNER_TEMP/cores，目录对所有用户可写。sudo 默认把 RLIMIT_CORE 设为 0，以 root
# 运行的用例须经 "${CRASH_SUDO[@]}" 启动以恢复 unlimited；root 进程的 core 归 root 且只有属主可读，由报告步骤用 sudo
# 读取。从 root 降权的子进程即使 fs.suid_dumpable 为 2（托管镜像的默认值）也不写出 core，实测如此，不在覆盖范围内。
# macOS：系统的 ReportCrash 自动写带原生调用栈的诊断报告，不开 core（macOS 的 core 动辄数 GB）；只留一个时间标记，
# 报告时只取本 job 内产生的诊断报告。
# Linux 容器 job（工作流设置 CRASH_CORE_DIR）：core_pattern 属于宿主机内核，在容器内只读；托管机器默认交给宿主机的
# systemd-coredump，按崩溃进程的 uid 加读取 ACL 并以 zstd 压缩存放。工作流把宿主机的 /var/lib/systemd/coredump 只读
# 挂到 CRASH_CORE_DIR，这里同样只留时间标记，报告时只取本 job 内产生的 core。
if [ -n "${CRASH_CORE_DIR:-}" ]; then
  [ -e "$RUNNER_TEMP/crash-marker" ] || touch "$RUNNER_TEMP/crash-marker"
  ulimit -c unlimited
elif [ "$RUNNER_OS" = Linux ]; then
  mkdir -p "$RUNNER_TEMP/cores"
  chmod 1777 "$RUNNER_TEMP/cores"
  pattern="$RUNNER_TEMP/cores/core.%e.%p"
  if [ "$(cat /proc/sys/kernel/core_pattern)" != "$pattern" ]; then
    sudo sysctl -q -w kernel.core_pattern="$pattern"
  fi
  ulimit -c unlimited
  # 用法："${CRASH_SUDO[@]}" <命令> <参数...>；由 source 本脚本的步骤使用，$0 与 $@ 须在 root 的 sh 中展开。
  # shellcheck disable=SC2034,SC2016
  CRASH_SUDO=(sudo sh -c 'ulimit -c unlimited; exec "$0" "$@"')
elif [ ! -e "$RUNNER_TEMP/crash-marker" ]; then
  touch "$RUNNER_TEMP/crash-marker"
fi
