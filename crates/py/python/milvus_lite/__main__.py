"""模块入口与安装后的 console script 使用同一参数和生命周期实现。"""

from milvus_lite.cmdline import main

if __name__ == "__main__":
    raise SystemExit(main())
