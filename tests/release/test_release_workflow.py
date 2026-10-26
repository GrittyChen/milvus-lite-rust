"""发行工作流的签发前保全门闩（0022）：环境、依赖、权限与「保全回执」→ sign 的先后都锁死在工作流文本上。

开发环境不装 YAML 解析库，按工作流固定的缩进切分：jobs 下两格缩进为 job，六格的 "- " 开始一个步骤。每条约束另有一个
变异用例，确认改坏后能被发现。
"""

from pathlib import Path
import re
import shlex

import pytest

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
        if re.match(r"^  [a-z][a-z0-9-]*:\s*$", line):
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
