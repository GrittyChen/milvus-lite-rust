"""正式/FI 两种 PID 1、命名卷 LOCK、正常排空与监督器强杀恢复。"""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import uuid


def docker(*args, timeout=120):
    try:
        return subprocess.check_output(["docker", *map(str, args)], text=True, stderr=subprocess.STDOUT, timeout=timeout).strip()
    except subprocess.CalledProcessError as error:
        raise RuntimeError("Docker command failed: " + str(error.cmd) + "\n" + error.output) from error


def state(name):
    return json.loads(docker("inspect", name))[0]["State"]


def wait_log(name, marker, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = docker("logs", name)
        if marker in text:
            return text
        if not state(name)["Running"]:
            raise RuntimeError("container exited before " + marker + ": " + text)
        time.sleep(0.05)
    raise TimeoutError("missing PID 1 checkpoint: " + marker)


LOCK = """import fcntl,sys
with open('/data/service.db/LOCK','r+b') as f:
    try: fcntl.flock(f,fcntl.LOCK_EX|fcntl.LOCK_NB)
    except BlockingIOError: print('BLOCKED')
    else: print('FREE')
"""
RESTORE = """import json
from pathlib import Path
from pymilvus import MilvusClient
from milvus_lite.server_manager import server_manager_instance as manager
p='/data/service.db'
c=MilvusClient(p,dedicated=True)
try:
    c.load_collection('c')
    rows=list(c.get('c',[0,1,2,3],output_fields=['vector','category']))
    expected=%s
    assert rows==expected,(rows,expected)
    manifest=json.loads(Path(p,"collections/c/manifest.json").read_text())
    assert manifest["current_seq"]==%s,manifest
finally:
    c.close(); manager.release_server(p)
print('RESTORED')
"""


def verify_exit(result, forced):
    expected = 137 if forced else 0
    if result["Running"] or result["ExitCode"] != expected or result["OOMKilled"]:
        raise ValueError("wrong termination: " + repr(result))


def has_supervisor_kill(events, name):
    return any(e.get("Type") == "container" and e.get("Action") == "kill" and e.get("Actor", {}).get("Attributes", {}).get("name") == name and e["Actor"]["Attributes"].get("signal") == "9" for e in map(json.loads, events.splitlines()))


def build_image(wheel, tag, base, log):
    with tempfile.TemporaryDirectory() as temp:
        path = Path(temp)
        shutil.copy2(wheel, path / Path(wheel).name)
        for name in ["Dockerfile.pid1", "pid1_entry.py"]:
            shutil.copy2(Path(__file__).with_name(name), path / name)
        log.write_text(docker("build", "--build-arg", "BASE_IMAGE=" + base, "-f", path / "Dockerfile.pid1", "-t", tag, path, timeout=900) + "\n")


def probe(formal, fi, base, output):
    from pymilvus import MilvusClient
    output = Path(output)
    output.mkdir(parents=True, exist_ok=False)
    import hashlib
    identity = {"base_image": base, "base_image_id": json.loads(docker("image", "inspect", base))[0]["Id"], "docker": docker("version", "--format", "{{.Server.Version}}"), "formal_wheel_sha256": hashlib.sha256(Path(formal).read_bytes()).hexdigest(), "fi_wheel_sha256": hashlib.sha256(Path(fi).read_bytes()).hexdigest()}
    (output / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    token = "mlr-pid1-" + uuid.uuid4().hex[:10]
    images = {"formal": token + "-formal", "fi": token + "-fi", "base": token + "-base"}
    containers, volumes, records = [], [], []
    try:
        docker("tag", base, images["base"])
        build_image(formal, images["formal"], images["base"], output / "formal-build.log")
        build_image(fi, images["fi"], images["base"], output / "fi-build.log")
        for mode in ["normal", "drain", "kill"]:
            name = token + "-" + mode
            data, control = name + "-data", name + "-control"
            for volume in [data, control]:
                docker("volume", "create", volume)
                volumes.append(volume)
            image = images["formal" if mode == "normal" else "fi"]
            command = ["run", "-d", "--name", name, "-v", data + ":/data", "-v", control + ":/control", "-p", "127.0.0.1::19530"]
            if mode != "normal":
                command += ["--entrypoint", "python"]
            command += [image] + (["-u", "/opt/pid1_entry.py"] if mode != "normal" else [])
            containers.append(name)
            docker(*command)
            wait_log(name, "listening on ")
            cmdline = docker("exec", name, "python", "-c", "from pathlib import Path;print(Path('/proc/1/cmdline').read_bytes().decode().replace(chr(0),' '))")
            if ("milvus-lite server" not in cmdline if mode == "normal" else "pid1_entry.py" not in cmdline):
                raise ValueError("unexpected PID 1 command: " + cmdline)
            mapping = docker("port", name, "19530/tcp")
            client = MilvusClient(uri="http://" + mapping, dedicated=True)
            try:
                client.create_collection("c", dimension=2, metric_type="L2")
                for pk in range(4):
                    client.insert("c", [{"id": pk, "vector": [float(pk + 1), 0.0], "category": "tech"}])
                    client.flush("c")
                expected = [{"id": pk, "vector": [float(pk + 1), 0.0], "category": "tech"} for pk in range(4)]
                if mode == "normal":
                    client.upsert("c", [{"id": 0, "vector": [0.0, 1.0], "category": "ai"}])
                    client.delete("c", ids=[1])
                    expected = [{"id": 0, "vector": [0.0, 1.0], "category": "ai"}, *expected[2:]]
            finally:
                client.close()
            kernel = docker("exec", name, "uname", "-r")
            def locked(expected_status):
                got = docker("run", "--rm", "-v", data + ":/data", "--entrypoint", "python", images["formal"], "-c", LOCK)
                if got != expected_status:
                    raise ValueError("unexpected LOCK state: " + got)
            locked("BLOCKED")
            if mode == "normal":
                docker("stop", "--time", "30", name)
            else:
                wait_log(name, "PAUSED")
                docker("kill", "--signal", "SIGTERM", name)
                wait_log(name, "DEFAULT")
                locked("BLOCKED")
                if mode == "drain":
                    docker("exec", name, "python", "-c", "from pathlib import Path;Path('/control/resume').touch()")
                    docker("wait", name, timeout=30)
                else:
                    docker("kill", "--signal", "SIGTERM", name)
                    if not state(name)["Running"] or "FINISHED" in docker("logs", name):
                        raise ValueError("PID 1 did not retain the default-signal boundary")
                    event_start = docker("exec", name, "date", "+%s")
                    docker("stop", "--time", "1", name)
                    events = docker("events", "--since", event_start, "--until", str(int(time.time()) + 1), "--filter", "container=" + name, "--format", "{{json .}}")
                    (output / (mode + "-events.jsonl")).write_text(events + "\n")
                    if not has_supervisor_kill(events, name):
                        raise ValueError("missing supervisor SIGKILL event")
            result = state(name)
            verify_exit(result, mode == "kill")
            text = docker("logs", name)
            (output / (mode + ".log")).write_text(text + "\n")
            if text.count("Shutting down...") != 1:
                raise ValueError("wrong shutdown output")
            locked("FREE")
            restored = docker("run", "--rm", "-v", data + ":/data", "--entrypoint", "python", images["formal"], "-c", RESTORE % (repr(expected), 6 if mode == "normal" else 4), timeout=60)
            if restored != "RESTORED":
                raise ValueError("unexpected recovery output: " + restored)
            records.append({"mode": mode, "exit": result["ExitCode"], "kernel": kernel, "pid1_command": cmdline, "recovered_rows": len(expected), "passed": True})
    finally:
        for name in containers:
            if docker("ps", "-aq", "--filter", "name=^" + name + "$"):
                docker("rm", "-f", name)
        for volume in volumes:
            docker("volume", "rm", volume)
        for image in images.values():
            # 清理由本次 token 识别的镜像，不操作其它工作容器。
            existing = docker("image", "ls", "-q", image)
            if existing:
                docker("image", "rm", image)
    return records


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--formal-wheel", required=True, type=Path)
    parser.add_argument("--fi-wheel", required=True, type=Path)
    parser.add_argument("--image", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    print(json.dumps(probe(args.formal_wheel, args.fi_wheel, args.image, args.output)))


if __name__ == "__main__":
    main()
