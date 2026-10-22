"""读取 cargo --message-format=json 的输出，要求全部产物为 fresh（0019 P0 第 3 条的预构建核对）。"""
import json
import sys


def main(path):
    artifacts = []
    for line in open(path, encoding="utf-8"):
        if line.startswith("{"):
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact":
                artifacts.append(message)
    stale = [a["target"]["name"] for a in artifacts if not a["fresh"]]
    if not artifacts or stale:
        raise SystemExit(f"{path}：{len(artifacts)} 个产物，非 fresh：{stale}")
    print(f"{path}：{len(artifacts)} 个产物全部 fresh")


if __name__ == "__main__":
    main(sys.argv[1])
