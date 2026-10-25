"""容器 job 的 artifact 路径约束：upload-artifact / download-artifact 的多行 path 只有第一行会换算成容器内路径，
其余行仍指向宿主机而找不到文件，失败记录因此从未上传（if-no-files-found: ignore 不报错）。容器 job 只能给一个路径。

开发环境不装 YAML 解析库，按工作流文件固定的两格缩进逐行解析：jobs 下两格缩进为 job，四格的 container 键表示容器 job，
六格的 "- " 开始一个步骤。
"""

from pathlib import Path
import re

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"


def multiline_container_paths(text):
    """返回 [(job, 步骤序号)]：容器 job 中 path 为块标量（多行）的 artifact 步骤。"""
    found = []
    job, container, steps = None, False, []

    def flush():
        if job and container:
            found.extend((job, index) for index, step in enumerate(steps)
                         if re.search(r"uses: actions/(upload|download)-artifact@", step)
                         and re.search(r"^\s+path:\s*[|>]", step, re.M))

    in_jobs = False
    for line in text.splitlines():
        if re.match(r"^jobs:\s*$", line):
            in_jobs = True
            continue
        if not in_jobs:
            continue
        if re.match(r"^\S", line):
            break
        name = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
        if name:
            flush()
            job, container, steps = name.group(1), False, []
            continue
        if re.match(r"^    container:", line):
            container = True
        if re.match(r"^      - ", line):
            steps.append("")
        if steps:
            steps[-1] += line + "\n"
    flush()
    return found


def test_container_jobs_give_artifact_steps_a_single_path():
    files = sorted(WORKFLOWS.glob("*.yml"))
    assert files
    for path in files:
        assert multiline_container_paths(path.read_text()) == [], path.name


def test_detects_multiline_path_only_in_container_jobs():
    text = """name: x
jobs:
  boxed:
    runs-on: ubuntu-24.04
    container:
      image: example
    steps:
      - uses: actions/checkout@v1
      - name: 保存
        uses: actions/upload-artifact@v1
        with:
          path: |
            a
            b
      - uses: actions/upload-artifact@v1
        with:
          path: single
  host:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/upload-artifact@v1
        with:
          path: |
            a
            b
"""
    assert multiline_container_paths(text) == [("boxed", 1)]
