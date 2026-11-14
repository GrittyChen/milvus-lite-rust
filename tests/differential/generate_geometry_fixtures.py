"""导出 GEOMETRY 的组件真值：真实 3.x（1320d26）的 `milvus_lite/schema/geometry.py` 与 CPython 3.12 的 `math.hypot`。

内容（浮点数一律写成位模式的十六进制，NaN 写成 "nan"）：
- templates：把一个位置换成任意码位（代理区除外）后仍被 `parse_wkt` 接受的码位全集，覆盖空白、关键字的每个字母、
  坐标里的字符与分隔符。Rust 一侧同样遍历全部码位，接受的集合必须完全相同。
- parse：WKT 各种写法的解析结果（不接受为 null），含每一组 Unicode 十进制数字。
- shapes / contains / intersects / distances：一组几何两两组合的 `geometry_contains`、`geometry_intersects` 与
  `_geometry_distance`；`geometry_within(a, b)` 就是 `contains(b, a)`。
- dwithin：`geometry_dwithin` 在距离附近的阈值上的结果。
- reference：参照实现 tests/search/filter/test_geometry.py 里直接调用这些函数的全部断言；生成时核对 3.x 的结果
  与用例写的期望相同。
- hypot：`math.hypot` 在 0、次正规数、接近溢出、非有限值与随机输入上的结果。

--verify 只比较，不改写语料文件。
"""
import json
import math
import random
import struct
import sys
import unicodedata
from pathlib import Path

from milvus_lite.schema import geometry as g

assert sys.version_info[:2] == (3, 12), sys.version
assert unicodedata.unidata_version == "15.0.0", unicodedata.unidata_version
target = Path(__file__).resolve().parents[2] / "crates/core/tests/fixtures/geometry.json"

SQUARE = "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))"
U_SHAPE = "POLYGON((0 0, 4 0, 4 4, 3 4, 3 1, 1 1, 1 4, 0 4, 0 0))"
CROSSING = "POLYGON((0.5 0.5, 3.5 0.5, 3.5 3.5, 0.5 3.5, 0.5 0.5))"
CONTAINED = "POLYGON((0.2 0.2, 0.8 0.2, 0.8 0.8, 0.2 0.8, 0.2 0.2))"
SMALL = "POLYGON((0 0, 2 0, 2 2, 0 2, 0 0))"
RING = "((0 0,1 0,1 1,0 0))"


def bits(value):
    return "nan" if value != value else format(struct.unpack("<Q", struct.pack("<d", value))[0], "016x")


def parsed(text):
    try:
        shape = g.parse_wkt(text)
    except g.SchemaValidationError:
        return None
    points = [shape] if isinstance(shape, g.Point) else shape.points
    return {"polygon": isinstance(shape, g.Polygon), "points": [[bits(p.x), bits(p.y)] for p in points]}


def templates():
    # 每一项是 (前缀, 后缀)，中间放一个任意码位。
    items = [
        ("", "POINT(1 1)"), ("POINT", "(1 1)"), ("POINT(", "1 1)"), ("POINT(1", "2)"), ("POINT(1 2", ")"), ("POINT(1 2)", ""),
        ("POINT(", " 1)"), ("POINT(1", " 1)"), ("POINT(1e", "1 1)"), ("POINT(", "1 1)"),
        ("", "POLYGON" + RING), ("POLYGON", RING), ("POLYGON(", RING[1:]), ("POLYGON((0", "0,1 0,1 1,0 0))"),
        ("POLYGON((0 0", "1 0,1 1,0 0))"), ("POLYGON((0 0,", "1 0,1 1,0 0))"), ("POLYGON((0 0,1 0,1 1,0 0", "))"),
        ("POLYGON((0 0,1 0,1 1,0 0)", ")"), ("POLYGON((0 0,1 0,1 1,0 0))", ""), ("POLYGON((0 0,1 0,1 1,0 ", "))"),
    ]
    for word, tail in (("POINT", "(1 1)"), ("POLYGON", RING)):
        items += [(word[:i], word[i + 1:] + tail) for i in range(len(word))]
    points = [chr(p) for p in range(0x110000) if not 0xD800 <= p <= 0xDFFF]
    return [
        {"before": before, "after": after, "accepted": [ord(c) for c in points if g.geometry_is_valid(before + c + after)]}
        for before, after in dict.fromkeys(items)
    ]


def parse_texts():
    texts = [
        "POINT(1 1)", "POINT(20 20)", SQUARE, "  point ( 1e1   -2.5 )  ", "POINT(1 1)\n", "POINT(1 1)\n\n", "\tPoInT\t(\t1\t2\t)\t",
        "POINT(+1 -.5)", "POINT(1. 1.)", "POINT(-0 -0.0)", "POINT(1e-400 1)", "POINT(1e308 -1e308)", "POINT(1E2 1)",
        "POINT(1_0 1)", "POINT(1e1_0 1)", "POINT(1_0.0_1e0_1 1)", "POINT(0x10 1)", "POINT(1__0 1)", "POINT(1_ 1)", "POINT(_1 1)",
        "POINT(.e1 1)", "POINT(1e 1)", "POINT(+-1 1)", "POINT(. 1)", "POINT(1e+ 1)", "POINT(1e+5 1e-5)", "POINT(00012 -00.50)",
        "POINT(inf 1)", "POINT(-inf 1)", "POINT(nan 1)", "POINT(infinity 1)", "POINT(-Infinity 1)", "POINT(NaN 1)", "POINT(1e400 1)",
        "POINT(1 -1e400)", "POINT(1.7976931348623157e308 1.7976931348623159e308)", "POINT(4.9e-324 2.4e-324)",
        "POINT(0.1 0.30000000000000004)", "POINT(9007199254740993 123456789012345678901234567890)",
        "POINT(1)", "POINT(1 2 3)", "POINT(1,2)", "POINT((1 2))", "POINT(1 2", "POINT 1 2)", "POINT()", "POINT( )", "POINT EMPTY",
        "POINT Z(1 1 1)", "POINT Z (1 2 3)", "xPOINT(1 1)", "POINT(1 1)x", "POINT(1 1)\x00", "\x00POINT(1 1)", "POINT(1 1))",
        "POINT((1 1)", "SRID=4326;POINT(1 1)", "MULTIPOINT(1 1)", "LINESTRING(0 0,1 1)", "LINESTRING(0 0, 1 1)",
        "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)))", "", " ", "not-wkt", "POINT", "POLYGON", "POINTPOINT(1 1)", "POINT(1 1)POINT(1 1)",
        "POINT(1 2)\nPOINT(1 2)", "POINT(1\n2)", "POINT(1 2)\r\n", "\n\nPOINT(1 2)", "POINT(nan 2)", "POINT(inf 2)", "POINT(1 2)",
        "polygon ( ( 0 0 ,1 0, 1 1 , 0 0 ) ) \n", "POLYGON((0 0,0 0,0 0,0 0))", "POLYGON((0 0,1 0,1 1,-0 -0.0))",
        "POLYGON((0 0,4 0,4 4,2 1,0 4,0 0))", "POLYGON((0 0,1 0,1 1,0 0,))", "POLYGON(())", "POLYGON(( ))", "POLYGON((0 0,1 0,0 0))",
        "POLYGON((0 0,1 0,1 1,0 1))", "POLYGON((0 0,2 2,2 0,0 2,0 0))", "POLYGON((0 0, 2 2, 0 2, 2 0, 0 0))",
        "POLYGON((0 0,1 0,1 1,0 0,0 0))", "POLYGON((0 0,4 0,4 4,0 4,0 0),(1 1,2 1,2 2,1 2,1 1))",
        "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 1))", "POLYGON((0 0,1 0,1 1,0 0)", "POLYGON(0 0,1 0,1 1,0 0)",
        "POLYGON((0 0 0,1 0 0,1 1 0,0 0 0))", "POLYGON((0 0,1 0,,1 1,0 0))", "POLYGON((0 0,1 0,1 inf,0 0))",
        "POLYGON((0 0,1 0,1 1,0 0)) x", "POLYGON((0 0,1e-10 0,1 1,0 0))", "POLYGON((0 0,2 0,1 0,1 1,0 0))",
        "POLYGON((0 0,2 0,2 2,1 0,0 2,0 0))", "POLYGON((0 0,1 0,1 1,0 0))\n\n", "POLYGON((\n0 0,\n1 0,\n1 1,\n0 0\n))",
        "POLYGON((0 0,1 0,1 1,0 0)))", "POLYGON(((0 0,1 0,1 1,0 0))", "POLYGON((0 0,1 0,1 1,0 0) )", "POLYGON( (0 0,1 0,1 1,0 0))",
        "POLYGON((0 0,1 0,1 1,0 0),)", "POLYGON((0 0,1 0,1 1,1e-10 0))", "POLYGON((0 0,1 0,1 1,0 1e-400))",
        "POLYGON((0 0,1e308 0,1e308 1e308,0 1e308,0 0))", "POLYGON((0 0,10 0,10 10,5 0,0 10,0 0))",
        "POLYGON((0 0,10 0,10 10,5 1e-10,0 10,0 0))", "POLYGON((0 0,4 0,4 4,4 0,0 0))", "POLYGON((0 0,4 0,8 0,4 0,0 0))",
        U_SHAPE, CROSSING, CONTAINED, "POLYGON EMPTY", "POLYGON Z((0 0 0,1 0 0,1 1 0,0 0 0))",
    ]
    # 源码里不写不可见字符：空白、I 的两个大小写等价码位、不算空白的相近码位、全角与阿拉伯数字都用码位构造。
    for point in (0x1C, 0x1F, 0x85, 0xA0, 0x1680, 0x2000, 0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000, 0x180E, 0x200B, 0xFEFF):
        c = chr(point)
        texts += [f"POINT(1{c}2)", f"{c}POINT{c}({c}1 2{c}){c}", f"POLYGON((0{c}0,1 0{c},{c}1 1,0 0{c}){c})"]
    for point in (0x130, 0x131, 0x17F, 0x212A, 0xFF29, 0x1D5C):
        c = chr(point)
        texts += [f"PO{c}NT(1 1)", f"POLYGON{RING}".replace("I", c), f"POLYGON{RING}".replace("O", c)]
    arabic, wide = "".join(chr(0x660 + d) for d in range(10)), "".join(chr(0xFF10 + d) for d in range(10))
    texts += [f"POINT({arabic[1]} {arabic[2]})", f"POINT({wide[1]}{wide[2]} 1)", f"POINT({arabic[1]}_{arabic[2]} 1)",
              f"POINT(1{arabic[5]}.{wide[5]}e{arabic[1]} -{wide[7]})", f"POINT({chr(0xB2)} 1)", f"POINT({chr(0xBD)} 1)",
              f"POINT({chr(0x2460)} 1)", f"POINT({chr(0x4E00)} 1)", f"POINT(1{chr(0xFF0E)}5 1)", f"POINT({chr(0x2212)}1 1)"]
    digits = [chr(p) for p in range(0x110000) if unicodedata.category(chr(p)) == "Nd"]
    assert len(digits) % 10 == 0
    for start in range(0, len(digits), 10):
        block = digits[start:start + 10]
        assert [unicodedata.digit(c) for c in block] == list(range(10))
        texts.append(f"POINT({''.join(block[1:] + block[:1])} -{block[7]}.{block[5]})")
    return list(dict.fromkeys(texts))


SHAPES = [
    "POINT(1 1)", "POINT(5 5)", "POINT(10 5)", "POINT(13 14)", "POINT(2 3)", "POINT(0.3 0.1)", "POINT(1e308 1e308)",
    "POINT(-1e308 -1e308)", "POINT(2 2)", "POINT(15 5)", "POINT(0 5)", "POINT(4 5)", "POINT(4 1)", "POINT(4.1 1)", "POINT(20 20)",
    "POINT(10.0000000005 5)", "POINT(10.000000002 5)", "POINT(5 1e-10)", "POINT(5 -2e-9)", "POINT(2 1)", "POINT(-0 0)",
    SQUARE, "POLYGON((2 2, 4 2, 4 4, 2 4, 2 2))", "POLYGON((8 8, 12 8, 12 12, 8 12, 8 8))", "POLYGON((20 20, 30 20, 30 30, 20 20))",
    "POLYGON((0 0, 4 0, 4 4, 2 1, 0 4, 0 0))", "POLYGON((0 0,0 0,0 0,0 0))", "POLYGON((10 0, 20 0, 20 10, 10 10, 10 0))",
    "POLYGON((0.1 0.2, 0.7 0.3, 0.4 0.9, 0.1 0.2))", "POLYGON((0 0, 5 0, 5 5, 0 5, 0 0))", U_SHAPE, CROSSING, CONTAINED, SMALL,
    "POLYGON((1 1, 3 1, 3 3, 1 3, 1 1))", "POLYGON((3 3, 4 3, 4 4, 3 4, 3 3))", "POLYGON((20 20, 30 20, 30 30, 20 30, 20 20))",
    "POLYGON((0 10, 10 10, 10 20, 0 20, 0 10))", "POLYGON((10.0000000005 0, 20 0, 20 10, 10.0000000005 10, 10.0000000005 0))",
    "POLYGON((11 0, 20 0, 20 10, 11 10, 11 0))", "POLYGON((1e308 0, 0 1e308, -1e308 0, 0 -1e308, 1e308 0))",
    "POLYGON((0 0, 1e-200 0, 1e-200 1e-200, 0 0))", "POLYGON((-5 -5, 15 -5, 15 15, -5 15, -5 -5))",
    "POLYGON((0 0, 10 0, 10 10, 0 0))", "POLYGON((5 5, 6 5, 6 5, 5 5))",
]

# 参照实现 tests/search/filter/test_geometry.py 中直接调用几何函数的全部断言：(函数, 左, 右, 距离, 期望)。
REFERENCE = [
    ("contains", SQUARE, "POINT(5 5)", None, True),
    ("contains", SQUARE, "POINT(15 5)", None, False),
    ("within", "POINT(5 5)", SQUARE, None, True),
    ("intersects", SQUARE, "POINT(0 5)", None, True),
    ("contains", "POINT(1 1)", "POINT(1 1)", None, True),
    ("contains", "POINT(1 1)", "POINT(2 2)", None, False),
    ("contains", SQUARE, "POINT(0 5)", None, True),
    ("contains", SQUARE, SQUARE, None, True),
    ("contains", SQUARE, "POLYGON((0 0, 5 0, 5 5, 0 5, 0 0))", None, True),
    ("contains", U_SHAPE, CROSSING, None, False),
    ("contains", U_SHAPE, CONTAINED, None, True),
    ("within", "POINT(0 5)", SQUARE, None, True),
    ("within", "POINT(15 5)", SQUARE, None, False),
    ("intersects", "POINT(1 1)", "POINT(1 1)", None, True),
    ("intersects", "POINT(1 1)", "POINT(2 2)", None, False),
    ("intersects", "POINT(0 5)", SQUARE, None, True),
    ("intersects", SMALL, "POLYGON((1 1, 3 1, 3 3, 1 3, 1 1))", None, True),
    ("intersects", SMALL, "POLYGON((3 3, 4 3, 4 4, 3 4, 3 3))", None, False),
    ("dwithin", "POINT(1 1)", "POINT(4 5)", 5, True),
    ("dwithin", "POINT(1 1)", "POINT(4 5)", 4.999, False),
    ("dwithin", "POINT(1 1)", "POINT(1 1)", 0, True),
    ("dwithin", SMALL, "POINT(4 1)", 2, True),
    ("dwithin", SMALL, "POINT(4.1 1)", 2, False),
    # test_geometry_filter_evaluates / test_geometry_dwithin_filter_evaluates 的数据行。
    ("contains", "POLYGON((20 20, 30 20, 30 30, 20 30, 20 20))", "POINT(5 5)", None, False),
    ("dwithin", "POINT(10 10)", "POINT(1 1)", 5, False),
]
FUNCTIONS = {"contains": g.geometry_contains, "within": g.geometry_within, "intersects": g.geometry_intersects}


def reference():
    out = []
    for name, left, right, distance, expected in REFERENCE:
        got = g.geometry_dwithin(left, right, distance) if name == "dwithin" else FUNCTIONS[name](left, right)
        assert got is expected, (name, left, right, distance)
        out.append({"op": name, "left": left, "right": right, "distance": None if distance is None else bits(float(distance)),
                    "expected": expected})
    return out


def dwithin():
    out = []
    for i, a in enumerate(SHAPES):
        for j, b in enumerate(SHAPES):
            d = g._geometry_distance(g.parse_wkt(a), g.parse_wkt(b))
            if (i * 37 + j) % 5 or not 0 < d < math.inf:
                continue
            edge = d - 1e-9
            for limit in (d, edge, math.nextafter(edge, -math.inf), math.nextafter(edge, math.inf), d / 2, d * 2, math.inf):
                if limit >= 0:
                    out.append([i, j, bits(limit), g.geometry_dwithin(a, b, limit)])
    return out


def hypot():
    tiny = 2.2250738585072014e-308
    edges = [0.0, -0.0, 5e-324, tiny, math.nextafter(tiny, 0.0), 1.0, 3.0, -4.0, 1e-200, 1e200, 1.3407807929942596e154,
             1.7976931348623157e308, math.inf, -math.inf, math.nan]
    rng = random.Random(34)
    pairs = [(x, y) for x in edges for y in edges]
    for _ in range(600):
        pairs.append(struct.unpack("<dd", struct.pack("<QQ", rng.getrandbits(64), rng.getrandbits(64))))
    for _ in range(900):
        x = math.ldexp(rng.uniform(1, 2), rng.randrange(-1074, 1024))
        pairs.append((x, x * rng.uniform(-4, 4) * rng.choice([1.0, 1.0, 1e-8, 1e8, 1e-17])))
    return [[bits(x), bits(y), bits(math.hypot(x, y))] for x, y in pairs]


shapes = [g.parse_wkt(text) for text in SHAPES]
sections = {
    "templates": templates(),
    "parse": [{"text": text, "result": parsed(text)} for text in parse_texts()],
    "shapes": SHAPES,
    "contains": ["".join("01"[g.geometry_contains(a, b)] for b in SHAPES) for a in SHAPES],
    "intersects": ["".join("01"[g.geometry_intersects(a, b)] for b in SHAPES) for a in SHAPES],
    "distances": [[bits(g._geometry_distance(a, b)) for b in shapes] for a in shapes],
    "dwithin": dwithin(),
    "reference": reference(),
    "hypot": hypot(),
}
for a in SHAPES:
    for b in SHAPES:
        assert g.geometry_within(a, b) == g.geometry_contains(b, a)
text = "{\n" + ",\n".join(f"{json.dumps(key)}: {json.dumps(value, ensure_ascii=True)}" for key, value in sections.items()) + "\n}\n"
if "--verify" in sys.argv:
    assert target.read_text(encoding="ascii") == text
else:
    target.write_text(text, encoding="ascii")
