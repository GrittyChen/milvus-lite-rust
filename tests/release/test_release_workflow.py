"""发行工作流的签发前保全门闩（0022）：环境、依赖、权限与「保全回执」→ sign 的先后都锁死在工作流文本上；
提速编排（0039）：测试 job 直接取构建产物、依赖表、macOS 按平台限流、已测摘要的交叉核对与签发路径的边界。

开发环境不装 YAML 解析库，按工作流固定的缩进切分：jobs 下两格缩进为 job，六格的 "- " 开始一个步骤。每条约束另有一个
变异用例，确认改坏后能被发现。
"""

from pathlib import Path
import re
import shlex

import pytest

import candidate as c
import runs

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "release.yml"
ENVIRONMENT = "    environment: ${{ inputs.dry_run && 'dry-run-signing' || 'release-signing' }}"
ATTEST_NEEDS = "    needs: [candidate, preserve-digests]"
OWNER = "OWNER_ID: ${{ github.repository_owner_id }}"
FI_OUTPUT = "FI_DIGESTS: ${{ needs.preserve-digests.outputs.fi }}"
SUBJECTS = "          subject-path: ${{ steps.subjects.outputs.paths }}"
ATTEST_PERMISSIONS = ["      contents: read", "      actions: read", "      id-token: write", "      attestations: write"]
# 完整参数向量逐项比较：只比前缀会放过追加的 --mode=dry-run、--help 或重复选项（argparse 以后出现的为准）。
GATE_ARGV = ["python3", "tests/release/preserve.py", "gate", "--mode", "$MODE", "--environment", "release-signing",
             "--owner-id", "$OWNER_ID", "--repo", "$GITHUB_REPOSITORY", "--version", "$VERSION",
             "--candidate-dir", "$RUNNER_TEMP/candidate", "--fi-digests", "$FI_DIGESTS",
             "--run-id", "$GITHUB_RUN_ID", "--run-attempt", "$GITHUB_RUN_ATTEMPT"]
PRECHECK_ARGV = ["python3", "tests/release/runs.py", "--repo", "$GITHUB_REPOSITORY", "precheck", "--sha", "$GITHUB_SHA",
                 "--version", "$VERSION", "--mode", "$MODE", "--run-id", "$GITHUB_RUN_ID",
                 "--run-attempt", "$GITHUB_RUN_ATTEMPT", "--pid1-dry-run", "$PID1_DRY_RUN",
                 "--signing-environment", "release-signing", "--owner-id", "$OWNER_ID", "--out", "$RUNNER_TEMP/meta.json"]


def parse(text):
    head, body = text.split("\njobs:\n", 1)
    jobs, job = {}, None
    for line in body.splitlines():
        if re.match(r"^  [a-z][a-z0-9_-]*:\s*$", line):
            job = line.strip()[:-1]
            jobs[job] = {"head": [], "steps": []}
        elif job and line.startswith("      - "):
            jobs[job]["steps"].append([line])
        elif job and jobs[job]["steps"]:
            jobs[job]["steps"][-1].append(line)
        elif job:
            jobs[job]["head"].append(line)
    return head, jobs


def block(lines, key, indent):
    """head 中 key: 之后、缩进更深的连续行。"""
    prefix = " " * indent + key + ":"
    if prefix not in lines:
        return None
    out = []
    for line in lines[lines.index(prefix) + 1:]:
        if not line.startswith(" " * (indent + 2)):
            break
        out.append(line)
    return out


def step_named(steps, name):
    found = [i for i, step in enumerate(steps) if re.match(rf"^      - name: {re.escape(name)}\s*$", step[0])]
    return found[0] if len(found) == 1 else None


def unconditional(step):
    return not any(re.match(r"^\s+(if|continue-on-error):", line) for line in step)


def argument(step, flag):
    text = " ".join(step)
    return re.findall(rf"{re.escape(flag)} (\S+)", text)


def commands(step):
    """run 块中实际执行的命令：去掉注释与空行，合并以反斜杠续行的行。"""
    lines, inside = [], False
    for line in step:
        single = re.match(r"^        run: (?!\|)(.+)$", line)
        if single:
            return [single.group(1).strip()]
        if re.match(r"^        run: \|\s*$", line):
            inside = True
            continue
        if inside:
            if line.strip() and not line.startswith("          "):
                break
            lines.append(line.strip())
    found, current = [], ""
    for line in lines:
        if not line or line.startswith("#"):
            continue
        if line.endswith("\\"):
            current += line[:-1] + " "
            continue
        found.append(current + line)
        current = ""
    if current:
        found.append(current)
    return found


def sole_command(step, argv):
    """run 块恰为一条命令，不含任何会改变执行语义的 shell 结构，拆分后的参数向量逐项等于 argv。"""
    found = commands(step)
    if len(found) != 1 or re.search(r"[;&|`<>]|\$\(", found[0]):
        return False
    try:
        tokens = shlex.split(found[0])
    except ValueError:
        return False
    return tokens == argv


def problems(text):
    head, jobs = parse(text)
    found = []
    top = block(head.splitlines(), "permissions", 0)
    if top != ["  contents: read"]:
        found.append(f"workflow permissions {top}")
    if re.search(r"^\s+(deployments|contents): write", text, re.M) or re.search(r"^\s+deployments:", text, re.M):
        found.append("write or deployments permission")
    environments = [(name, line) for name, job in jobs.items() for line in job["head"] if line.startswith("    environment:")]
    if environments != [("attest", ENVIRONMENT)]:
        found.append(f"environments {environments}")
    attest, digests = jobs.get("attest", {"head": [], "steps": []}), jobs.get("preserve-digests", {"head": [], "steps": []})
    if attest["head"].count(ATTEST_NEEDS) != 1:
        found.append("attest does not need candidate and preserve-digests")
    if digests["head"].count("    needs: candidate") != 1:
        found.append("preserve-digests does not need candidate")
    for name, job in (("attest", attest), ("preserve-digests", digests)):
        if any(line.startswith("    if:") for line in job["head"]):
            found.append(f"{name} has a job condition")
    if block(digests["head"], "permissions", 4) != ["      contents: read"]:
        found.append(f"preserve-digests permissions {block(digests['head'], 'permissions', 4)}")
    if block(attest["head"], "permissions", 4) != ATTEST_PERMISSIONS:
        found.append(f"attest permissions {block(attest['head'], 'permissions', 4)}")
    if runs.ATTEST_JOB != "attest" or "    name: attest" not in attest["head"]:
        found.append("attest job name")
    steps = attest["steps"]
    recheck, gate, sign = (step_named(steps, name) for name in ("签发前复核", "保全回执", runs.SIGN_STEP))
    if None in (recheck, gate, sign) or not recheck < gate < sign or sign != gate + 1:
        found.append(f"attest step order {recheck, gate, sign}")
    else:
        if not unconditional(steps[gate]) or not unconditional(steps[sign]):
            found.append("保全回执 or sign is conditional")
        if SUBJECTS not in steps[sign]:
            found.append("sign subjects do not come from the recheck step")
        gate_step = steps[gate]
        if argument(gate_step, "--environment") != ["release-signing"] or not any(OWNER in line for line in gate_step) \
                or argument(gate_step, "--owner-id") != ['"$OWNER_ID"'] or not any(FI_OUTPUT in line for line in gate_step) \
                or argument(gate_step, "--fi-digests") != ['"$FI_DIGESTS"']:
            found.append("保全回执 does not pass the literal environment, owner and FI digests")
        if argument(gate_step, "--mode") != ['"$MODE"']:
            found.append("保全回执 mode")
        if not sole_command(gate_step, GATE_ARGV):
            found.append(f"保全回执 does not run exactly preserve.py gate: {commands(gate_step)}")
    precheck = jobs.get("precheck", {"steps": []})["steps"]
    checked = [step for step in precheck if "runs.py" in " ".join(step)]
    if len(checked) != 1 or argument(checked[0], "--signing-environment") != ["release-signing"] \
            or not any(OWNER in line for line in checked[0]) or argument(checked[0], "--owner-id") != ['"$OWNER_ID"']:
        found.append("precheck does not pass the literal environment and owner")
    elif not sole_command(checked[0], PRECHECK_ARGV):
        found.append(f"precheck does not run exactly runs.py precheck: {commands(checked[0])}")
    return found


def test_release_workflow_gate_shape():
    assert problems(WORKFLOW.read_text()) == []


def replace(old, new, count=1):
    def apply(text):
        assert text.count(old) >= count, old
        return text.replace(old, new, count)
    return apply


def drop(line):
    return replace(line + "\n", "")


def digests_need_precheck(text):
    start = text.index("  preserve-digests:\n")
    at = text.index("    needs: candidate\n", start)
    return text[:at] + "    needs: precheck\n" + text[at + len("    needs: candidate\n"):]


def move_gate_after_sign(text):
    gate = re.search(r"      - name: 保全回执\n(?:(?!      - ).*\n)*", text).group(0)
    text = text.replace(gate, "")
    sign = re.search(r"      - name: sign\n(?:(?!      - ).*\n)*", text).group(0)
    return text.replace(sign, sign + gate)


@pytest.mark.parametrize("mutate", [
    replace(ENVIRONMENT, "    environment: release-signing"),
    replace(ENVIRONMENT, "    environment: ${{ inputs.environment }}"),
    drop(ENVIRONMENT),
    replace("  preserve-digests:\n    # ", "  preserve-digests:\n    environment: dry-run-signing\n    # "),
    replace(ATTEST_NEEDS, "    needs: candidate"),
    replace("  preserve-digests:\n    # ", "  preserve-digests:\n    if: always()\n    # "),
    replace("    name: attest\n", "    name: attest\n    if: always()\n"),
    digests_need_precheck,
    replace("    permissions:\n      contents: read\n    outputs:", "    permissions:\n      contents: read\n      actions: read\n    outputs:"),
    replace("      attestations: write\n", "      attestations: write\n      deployments: write\n"),
    replace("permissions:\n  contents: read\n", "permissions:\n  contents: write\n"),
    replace("    name: attest\n", "    name: sign-candidate\n"),
    move_gate_after_sign,
    replace("      - name: 保全回执\n", "      - name: 保全回执\n        if: env.MODE == 'release'\n"),
    replace("      - name: 保全回执\n", "      - name: 保全回执\n        continue-on-error: true\n"),
    replace("      - name: sign\n", "      - name: sign\n        if: always()\n"),
    replace("--environment release-signing", "--environment \"$SIGNING_ENVIRONMENT\""),
    replace("--fi-digests \"$FI_DIGESTS\"", "--fi-digests '{}'"),
    replace("          FI_DIGESTS: ${{ needs.preserve-digests.outputs.fi }}\n", "          FI_DIGESTS: '{}'\n"),
    replace("gate --mode \"$MODE\"", "gate --mode dry-run"),
    replace(SUBJECTS, "          subject-path: ${{ runner.temp }}/candidate/*"),
    replace("--signing-environment release-signing", "--signing-environment \"${{ inputs.environment }}\""),
    replace("      - name: 保全回执\n        # ", "      - name: 签发前检查\n        # "),
    replace("python3 tests/release/preserve.py gate", "true"),
    replace("python3 tests/release/preserve.py gate", "exit 0\n          python3 tests/release/preserve.py gate"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign",
            "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\" || true\n      - name: sign"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign",
            "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"; true\n      - name: sign"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign",
            "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n          true\n      - name: sign"),
    replace("python3 tests/release/preserve.py gate", "true && python3 tests/release/preserve.py gate"),
    replace("python3 tests/release/preserve.py gate", "python3 tests/release/preserve.py digests"),
    replace("python3 tests/release/preserve.py gate", "python3 tests/release/preserve.py verify gate"),
    replace("python3 tests/release/runs.py --repo \"$GITHUB_REPOSITORY\" precheck", "true"),
    replace("python3 tests/release/runs.py --repo \"$GITHUB_REPOSITORY\" precheck",
            "python3 tests/release/runs.py --repo \"$GITHUB_REPOSITORY\" recheck"),
    replace("--signing-environment release-signing --owner-id \"$OWNER_ID\" --out \"$RUNNER_TEMP/meta.json\"",
            "--signing-environment release-signing --owner-id \"$OWNER_ID\" --out \"$RUNNER_TEMP/meta.json\" || true"),
    replace("          python3 tests/release/runs.py --repo \"$GITHUB_REPOSITORY\" precheck",
            "          exit 0\n          python3 tests/release/runs.py --repo \"$GITHUB_REPOSITORY\" precheck"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign", "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\" --help\n      - name: sign"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign", "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\" -h\n      - name: sign"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign", "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\" --mode=dry-run\n      - name: sign"),
    replace("--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\"\n      - name: sign", "--run-id \"$GITHUB_RUN_ID\" --run-attempt \"$GITHUB_RUN_ATTEMPT\" --environment=other\n      - name: sign"),
    replace("--out \"$RUNNER_TEMP/meta.json\"", "--out \"$RUNNER_TEMP/meta.json\" --mode=dry-run"),
    replace("--out \"$RUNNER_TEMP/meta.json\"", "--out \"$RUNNER_TEMP/meta.json\" --signing-environment=other"),
    replace("--out \"$RUNNER_TEMP/meta.json\"", "--out \"$RUNNER_TEMP/meta.json\" --help"),
], ids=["env-literal", "env-input", "env-missing", "env-other-job", "needs", "digests-if", "attest-if", "digests-needs",
        "digests-permission", "deployments", "workflow-write", "attest-name", "gate-after-sign", "gate-if",
        "gate-continue", "sign-if", "gate-env", "gate-fi", "gate-fi-env", "gate-mode", "subjects", "precheck-env",
        "gate-name", "gate-true", "gate-exit-before", "gate-or-true", "gate-semicolon", "gate-extra-command",
        "gate-and-prefix", "gate-subcommand", "gate-verify", "precheck-true", "precheck-recheck", "precheck-or-true",
        "precheck-exit-before", "gate-help", "gate-h", "gate-mode-equals", "gate-env-equals", "precheck-mode-equals",
        "precheck-env-equals", "precheck-help"])
def test_gate_shape_mutations_detected(mutate):
    text = WORKFLOW.read_text()
    mutated = mutate(text)
    assert mutated != text
    assert problems(mutated)


# 0039 第 8 条的依赖表；candidate 依赖除签发两段之外的全部 job，由 speedup_problems 另行核对。
NEEDS = {"assemble": ["build", "build-mac-x86_64"], "tests": ["build", "orjson", "jieba"],
         "tests-mac-x86_64": ["build", "build-mac-x86_64", "orjson", "jieba"], "tests-mac-arm64": ["build", "orjson", "jieba"],
         # 其余验收 job 仍在装配之后（0039 第 8 条「不变」）。
         "linux-system": ["assemble"], "pid1": ["assemble"], "sdist": ["assemble", "identity"], "previous-reader": ["assemble"]}
MAC_TESTS = {"tests-mac-x86_64": ("macos-x86_64", "4"), "tests-mac-arm64": ("macos-arm64", "3")}
MAC_PARTS = "        part: [diff-2.6.18, diff-3.0.3, python, interop, static, fi]"
MODE_LINE = "  MODE: ${{ inputs.dry_run && 'dry-run' || 'release' }}"
VERSION_LINE = "  VERSION: ${{ inputs.version }}"
# YAML 锚点与别名：签发路径内若用别名，展开的内容在路径之外，摘要看不到它的变化。
ALIAS = re.compile(r"(?:^\s*-|:)\s+[&*][A-Za-z_]|<<:")


def needs(job):
    text = " ".join(line.strip() for line in job["head"])
    match = re.search(r"needs: (\[[^\]]*\]|\S+)", text)
    if match is None:
        return None
    value = match.group(1)
    return [x.strip() for x in value.strip("[]").split(",")] if value.startswith("[") else [value]


def scope_tags(job):
    """矩阵中各 platform 的 (scope, tag)。"""
    found, scope = [], None
    for line in job["head"]:
        if m := re.match(r"^          - scope: (\S+)$", line):
            scope = m.group(1)
        elif (m := re.match(r"^            tag: (\S+)$", line)) and scope:
            found.append((scope, m.group(1)))
    return found


def speedup_problems(text):
    head, jobs = parse(text)
    found = []
    for name, want in NEEDS.items():
        if needs(jobs.get(name, {"head": []})) != want:
            found.append(f"{name} needs {needs(jobs.get(name, {'head': []}))}")
    expected = sorted(set(jobs) - {"candidate", "preserve-digests", "attest"})
    if sorted(needs(jobs.get("candidate", {"head": []})) or []) != expected:
        found.append("candidate does not need every job before signing")
    tags = [pair for name in ("tests", *MAC_TESTS) for pair in scope_tags(jobs.get(name, {"head": []}))]
    if sorted(tags) != sorted(c.SCOPE_TAGS.items()):
        found.append(f"test matrix platforms and tags {tags} differ from candidate.SCOPE_TAGS")
    for name, (scope, limit) in MAC_TESTS.items():
        job = jobs.get(name, {"head": [], "steps": []})
        if f"      max-parallel: {limit}" not in job["head"] or MAC_PARTS not in job["head"] \
                or "    steps: *tests-steps" not in job["head"] or [s for s, _ in scope_tags(job)] != [scope]:
            found.append(f"{name} matrix, limit or steps")
    tests = jobs.get("tests", {"head": [], "steps": []})
    parts = [line for line in tests["head"] if line.startswith("        part: ")]
    if len(parts) != 1 or sorted(parts[0].split("[")[1].rstrip("]").split(", ")) != sorted(c.TEST_PARTS):
        found.append(f"tests parts {parts}")
    downloads = [line.strip() for step in tests["steps"] for line in step if re.match(r"^\s+name: \S", line)
                 and "download-artifact" in step[0]]
    if any(d in ("name: formal", "name: fi") for d in downloads) \
            or not {"name: build-${{ matrix.platform.scope }}", "name: build-linux-x86_64"} <= set(downloads):
        found.append(f"test jobs must take build artifacts, not the assembled ones: {downloads}")
    produced = tests["steps"][step_named(tests["steps"], "产物")] if step_named(tests["steps"], "产物") is not None else []
    if not any("tests/release/sums.py tested" in line for line in produced):
        found.append("test jobs do not record the files they used")
    candidate = " ".join(line for step in jobs.get("candidate", {"steps": []})["steps"] for line in step)
    if "tests/release/candidate.py tested" not in candidate:
        found.append("candidate does not cross-check the tested files")
    header = head.splitlines()
    if MODE_LINE not in header or VERSION_LINE not in header or [line for line in block(header, "defaults", 0) or [] if not line.strip().startswith("#")] != ["  run:", "    shell: bash"]:
        found.append("MODE, VERSION or defaults.run.shell changed")
    try:
        signing = runs.signing_jobs(text)
    except c.CandidateError as error:
        return found + [str(error)]
    env_keys = {m.group(1) for line in block(header, "env", 0) or [] if (m := re.match(r"^  ([A-Z][A-Z0-9_]*):", line))}
    for key in sorted(env_keys - set(runs.SIGNING_ENV_KEYS)):
        if re.search(rf"\$\{{?{key}\b|env\.{key}\b", signing):
            found.append(f"signing jobs read env {key}, which is outside the signing path")
    if any(ALIAS.search(line) for line in signing.splitlines()):
        found.append("signing jobs use a YAML anchor or alias")
    tools = set(re.findall(r"tests/release/[A-Za-z0-9_]+\.py", signing))
    if not tools <= set(c.SIGNING_ENTRIES) or ".github/ci/" in signing:
        found.append(f"signing jobs call tools outside the signing entries: {sorted(tools - set(c.SIGNING_ENTRIES))}")
    return found


def test_release_workflow_speedup_shape():
    assert speedup_problems(WORKFLOW.read_text()) == []


@pytest.mark.parametrize("mutate", [
    replace("    needs: [build, build-mac-x86_64]\n", "    needs: build\n"),
    replace("    needs: [build, orjson, jieba]\n    name: tests (${{ matrix.platform.scope }}, ${{ matrix.part }})\n    strategy:\n      fail-fast: false\n      matrix:\n        # ",
            "    needs: [assemble, orjson, jieba]\n    name: tests (${{ matrix.platform.scope }}, ${{ matrix.part }})\n    strategy:\n      fail-fast: false\n      matrix:\n        # "),
    replace("    needs: [build, build-mac-x86_64, orjson, jieba]\n", "    needs: [build, orjson, jieba]\n"),
    replace(" tests-mac-x86_64, tests-mac-arm64,\n", " tests-mac-x86_64,\n"),
    replace("      max-parallel: 4\n", "      max-parallel: 5\n"),
    replace("      max-parallel: 3\n", ""),
    replace("        part: [diff-2.6.18, diff-3.0.3, python, interop, static, fi]\n        platform:\n          - scope: macos-arm64",
            "        part: [diff-2.6.18, diff-3.0.3, interop, python, static, fi]\n        platform:\n          - scope: macos-arm64"),
    replace("            tag: macosx_11_0_arm64\n", "            tag: macosx_11_0_x86_64\n"),
    replace("          name: build-linux-x86_64\n          path: ${{ runner.temp }}/build-sdist",
            "          name: formal\n          path: ${{ runner.temp }}/build-sdist"),
    replace("          \"$DEV_PY\" tests/release/sums.py tested", "          true"),
    replace("python3 tests/release/candidate.py tested", "true"),
    replace(MODE_LINE, "  MODE: release"),
    replace("    shell: bash\n", "    shell: sh\n"),
    replace("--mode \"$MODE\" --version \"$VERSION\" \\\n            --candidate-dir",
            "--mode \"$MODE\" --version \"$PREVIOUS_VERSION\" \\\n            --candidate-dir"),
    replace("      - name: sign\n", "      - name: sign\n        env: &sign-env\n          X: y\n"),
    replace("          python3 tests/release/preserve.py gate", "          python3 tests/release/stage.py --help\n          python3 tests/release/preserve.py gate"),
    replace("          python3 tests/release/preserve.py gate", "          bash .github/ci/install_uv.sh\n          python3 tests/release/preserve.py gate"),
    lambda text: text + "  after:\n    runs-on: x\n",
    replace("    needs: [assemble, identity]\n", "    needs: [build, identity]\n"),
], ids=["assemble-needs", "tests-assemble", "intel-build", "candidate-needs", "intel-limit", "arm-limit", "arm-order",
        "tag", "download-formal", "no-tested", "no-cross-check", "mode", "shell", "env-key", "alias", "tool", "ci-script",
        "job-after-attest", "sdist-needs"])
def test_speedup_shape_mutations_detected(mutate):
    text = WORKFLOW.read_text()
    mutated = mutate(text)
    assert mutated != text
    assert speedup_problems(mutated)
