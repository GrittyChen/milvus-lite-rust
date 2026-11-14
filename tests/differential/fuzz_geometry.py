"""直接对照真实 3.x 的 geometry.py 与 Rust 的几何模块，不经服务、不依赖固定语料。

三类随机输入：WKT 文本（合法写法加空白、大小写、数字写法与结构上的变异）的解析结果；随机几何对的
`geometry_contains` / `geometry_within` / `geometry_intersects`、距离的位模式与阈值附近的 `geometry_dwithin`；
`math.hypot`。3.x 一侧抛出的任何异常都记进结果里参与比较。

运行前构建 cargo build -p milvus-lite-core --example geometry_probe。
脚本在 CPython 3.12 的参照环境运行，种子固定；不进入默认测试。
"""
import argparse
import json
import math
import random
import struct
import subprocess
import sys
from pathlib import Path

from milvus_lite.schema import geometry as g

assert sys.version_info[:2] == (3, 12), sys.version
ROOT = Path(__file__).resolve().parents[2]
SPACES = [chr(p) for p in (0x20, 0x20, 0x20, 0x09, 0x0A, 0x0D, 0x1C, 0x85, 0xA0, 0x1680, 0x2003, 0x2028, 0x3000)]
NEAR_SPACES = [chr(p) for p in (0x180E, 0x200B, 0xFEFF, 0x00)]
LETTERS = {"I": [chr(0x130), chr(0x131)], "O": [chr(0xFF2F)], "N": [chr(0x1D5C)]}
DIGITS = [0x30, 0x30, 0x30, 0x660, 0xFF10, 0x966, 0x1D7CE]
EPSILONS = [0.0, 0.0, 0.0, 1e-9, -1e-9, 5e-10, 2e-9, 1e-10, 1e-12, 1e-13, 1e-15]
HUGE = [1e308, -1e308, 1e200, 1e-200, 5e-324, 1.7976931348623157e308]


def bits(value):
    return "nan" if value != value else format(struct.unpack("<Q", struct.pack("<d", value))[0], "016x")


def unhex(text):
    return math.nan if text == "nan" else struct.unpack("<d", bytes.fromhex(text)[::-1])[0]


def space(rng, at_least=0):
    return "".join(rng.choice(SPACES) for _ in range(rng.choice([at_least, at_least, 1, 2])))


def number(rng):
    kind = rng.randrange(12)
    if kind < 5:
        text = str(rng.randrange(-12, 13))
    elif kind < 8:
        text = repr(rng.randrange(-12, 13) + rng.choice(EPSILONS) + rng.choice([0, 0.5, 0.25, 0.1]))
    elif kind == 8:
        text = repr(rng.uniform(-100, 100))
    elif kind == 9:
        text = repr(rng.choice(HUGE))
    else:
        text = rng.choice(["1e1", "1E-2", ".5", "5.", "+3", "-0", "-0.0", "1_0", "1e1_0", "0010", "1e400", "1e-400", "inf", "nan",
                           "-inf", "Infinity", "0x10", "1__0", "_1", "1_", "1e", ".", "+-1", "1.2.3", "--1", "1e+3", "", "1,2"])
    if rng.randrange(8) == 0:
        base = rng.choice(DIGITS)
        text = "".join(chr(base + ord(c) - 48) if c.isdigit() and c.isascii() else c for c in text)
    return text


def keyword(rng, word):
    out = []
    for letter in word:
        roll = rng.randrange(40)
        if roll < 12:
            out.append(letter.lower())
        elif roll == 12 and letter in LETTERS:
            out.append(rng.choice(LETTERS[letter]))
        else:
            out.append(letter)
    return "".join(out)


def wkt(rng):
    if rng.randrange(2):
        body = f"{space(rng)}{number(rng)}{space(rng, 1)}{number(rng)}{space(rng)}"
        if rng.randrange(12) == 0:
            body += f"{space(rng, 1)}{number(rng)}"
        text = f"{space(rng)}{keyword(rng, 'POINT')}{space(rng)}({body}){space(rng)}"
    else:
        count = rng.choice([1, 2, 3, 3, 4, 4, 5, 6])
        pairs = [(number(rng), number(rng)) for _ in range(count)]
        if rng.randrange(6):
            pairs.append(pairs[0])
        parts = [f"{space(rng)}{x}{space(rng, 1)}{y}{space(rng)}" for x, y in pairs]
        text = f"{space(rng)}{keyword(rng, 'POLYGON')}{space(rng)}({space(rng)}({','.join(parts)}){space(rng)}){space(rng)}"
    for _ in range(rng.choice([0, 0, 0, 1, 1, 2])):
        at = rng.randrange(len(text) + 1)
        roll = rng.randrange(6)
        if roll == 0:
            text = text[:at] + text[at + 1:]
        elif roll == 1:
            text = text[:at] + rng.choice(NEAR_SPACES + SPACES) + text[at:]
        elif roll == 2:
            text = text[:at] + rng.choice("(),. _eE+-xZ0") + text[at:]
        elif roll == 3:
            point = rng.randrange(0x110000)
            text = text[:at] + ("A" if 0xD800 <= point <= 0xDFFF else chr(point)) + text[at:]
        elif roll == 4:
            text = text[:at] + text[at:at + 1] + text[at:]
        else:
            text = text[:at] + rng.choice(SPACES) + text[at + 1:]
    return text


def coordinate(rng, scale):
    roll = rng.randrange(40)
    if roll == 0:
        return rng.choice(HUGE)
    if roll < 6:
        return rng.uniform(-scale, scale)
    return rng.randrange(-scale, scale + 1) * rng.choice([1, 1, 1, 0.5]) + rng.choice(EPSILONS)


def shape(rng):
    scale = rng.choice([2, 4, 4, 8])
    if rng.randrange(3) == 0:
        return f"POINT({coordinate(rng, scale)!r} {coordinate(rng, scale)!r})"
    points = [(coordinate(rng, scale), coordinate(rng, scale)) for _ in range(rng.choice([3, 3, 4, 4, 5, 6, 8]))]
    if rng.randrange(5):
        # 多数按极角排序成简单多边形；其余保持随机顺序，其中自交的由解析拒绝。
        cx, cy = sum(p[0] for p in points) / len(points), sum(p[1] for p in points) / len(points)
        points.sort(key=lambda p: math.atan2(p[1] - cy, p[0] - cx))
    points.append(points[0])
    return "POLYGON((" + ", ".join(f"{x!r} {y!r}" for x, y in points) + "))"


def reference(request):
    try:
        if "parse" in request:
            value = g.parse_wkt(request["parse"])
            points = [value] if isinstance(value, g.Point) else value.points
            return {"polygon": isinstance(value, g.Polygon), "points": [[bits(p.x), bits(p.y)] for p in points]}
        if "hypot" in request:
            x, y = (unhex(h) for h in request["hypot"])
            return bits(math.hypot(x, y))
        a, b = request["pair"]
        left, right = g.parse_wkt(a), g.parse_wkt(b)
        limits = [unhex(h) for h in request["limits"]]
        return {
            "contains": g.geometry_contains(a, b),
            "within": g.geometry_within(a, b),
            "intersects": g.geometry_intersects(a, b),
            "distance": bits(g._geometry_distance(left, right)),
            "dwithin": [g.geometry_dwithin(a, b, limit) for limit in limits],
        }
    except g.SchemaValidationError:
        return None
    except Exception as error:  # noqa: BLE001  其余异常本身就是要比较的结果
        return {"raised": type(error).__name__}


def pair(rng):
    a, b = shape(rng), shape(rng)
    limits = [rng.choice([0.0, 0.5, 1.0, 2.0, 1e-9, math.inf]), rng.uniform(0, 8)]
    try:
        d = g._geometry_distance(g.parse_wkt(a), g.parse_wkt(b))
    except Exception:  # noqa: BLE001
        d = math.nan
    if 0 < d < math.inf:
        edge = d - 1e-9
        limits += [v for v in (d, edge, math.nextafter(edge, -math.inf), math.nextafter(edge, math.inf)) if v >= 0]
    return {"pair": [a, b], "limits": [bits(v) for v in limits]}


def hypot(rng):
    if rng.randrange(3) == 0:
        x, y = struct.unpack("<dd", struct.pack("<QQ", rng.getrandbits(64), rng.getrandbits(64)))
    else:
        x = math.ldexp(rng.uniform(1, 2), rng.randrange(-1074, 1024))
        y = x * rng.uniform(-4, 4) * rng.choice([1.0, 1.0, 1e-8, 1e8, 1e-17])
    return {"hypot": [bits(x), bits(y)]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cases", type=int, default=200000, help="每一类的条数")
    parser.add_argument("--seed", type=int, default=34)
    parser.add_argument("--probe", default=str(ROOT / "target/debug/examples/geometry_probe"))
    parser.add_argument("--output", help="把完整报告写到这个文件（放仓库外）")
    args = parser.parse_args()
    rng = random.Random(args.seed)
    requests = [{"parse": wkt(rng)} for _ in range(args.cases)]
    requests += [pair(rng) for _ in range(args.cases)]
    requests += [hypot(rng) for _ in range(args.cases)]
    expected = [reference(request) for request in requests]
    stdin = "".join(json.dumps(request) + "\n" for request in requests)
    done = subprocess.run([args.probe], input=stdin, capture_output=True, text=True, check=True)
    actual = [json.loads(line) for line in done.stdout.splitlines()]
    assert len(actual) == len(requests)
    bad = [(r, e, a) for r, e, a in zip(requests, expected, actual) if e != a]
    kinds = {"parse": expected[:args.cases], "pair": expected[args.cases:2 * args.cases]}
    report = {
        "seed": args.seed,
        "cases": len(requests),
        "parse_accepted": sum(e is not None for e in kinds["parse"]),
        "pair_evaluated": sum(e is not None for e in kinds["pair"]),
        "pair_contains": sum(bool(e and e.get("contains")) for e in kinds["pair"]),
        "pair_intersects": sum(bool(e and e.get("intersects")) for e in kinds["pair"]),
        "raised": sum(bool(isinstance(e, dict) and "raised" in e) for e in expected),
        "mismatches": len(bad),
        "first": bad[:5],
    }
    text = json.dumps(report, ensure_ascii=True)
    if args.output:
        Path(args.output).write_text(text + "\n", encoding="ascii")
    print(text)
    raise SystemExit(1 if bad else 0)


main()
