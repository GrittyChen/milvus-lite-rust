//! 不经服务的几何对照入口。每行输入是一个 JSON 对象，输出一行 JSON；浮点数写成位模式的十六进制，NaN 写成 "nan"。
//!
//! - `{"parse": 文本}`：不接受时输出 `null`，否则 `{"polygon": 是否多边形, "points": [[x, y], ...]}`。
//! - `{"pair": [a, b], "limits": [上限, ...]}`：任一文本不接受时输出 `null`，否则输出
//!   `contains`、`within`、`intersects`、`distance` 与每个上限的 `dwithin`。
//! - `{"hypot": [x, y]}`：输出结果。
use milvus_lite_core::geometry::{Geometry, contains, distance, dwithin, hypot, intersects, parse};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

type Error = Box<dyn std::error::Error>;

fn bits(value: f64) -> String {
    if value.is_nan() {
        "nan".into()
    } else {
        format!("{:016x}", value.to_bits())
    }
}

fn float(value: &Value) -> Result<f64, Error> {
    Ok(match value.as_str().ok_or("floats are hex text")? {
        "nan" => f64::NAN,
        hex => f64::from_bits(u64::from_str_radix(hex, 16)?),
    })
}

fn text(value: &Value) -> Result<&str, Error> {
    Ok(value.as_str().ok_or("expected a string")?)
}

fn answer(request: &Value) -> Result<Value, Error> {
    if let Some(source) = request.get("parse") {
        return Ok(match parse(text(source)?) {
            None => Value::Null,
            Some(shape) => {
                let (polygon, points) = match shape {
                    Geometry::Point(p) => (false, vec![p]),
                    Geometry::Polygon(points) => (true, points),
                };
                let points: Vec<_> = points.iter().map(|p| [bits(p.x), bits(p.y)]).collect();
                json!({"polygon": polygon, "points": points})
            }
        });
    }
    if let Some(pair) = request.get("pair") {
        let (Some(a), Some(b)) = (parse(text(&pair[0])?), parse(text(&pair[1])?)) else {
            return Ok(Value::Null);
        };
        let limits = request["limits"].as_array().ok_or("limits")?;
        let within: Vec<bool> = limits
            .iter()
            .map(|limit| Ok(dwithin(&a, &b, float(limit)?)))
            .collect::<Result<_, Error>>()?;
        return Ok(json!({
            "contains": contains(&a, &b),
            "within": contains(&b, &a),
            "intersects": intersects(&a, &b),
            "distance": bits(distance(&a, &b)),
            "dwithin": within,
        }));
    }
    let pair = request.get("hypot").ok_or("unknown request")?;
    Ok(Value::String(bits(hypot(
        float(&pair[0])?,
        float(&pair[1])?,
    ))))
}

fn main() -> Result<(), Error> {
    let input = io::stdin().lock();
    let mut output = io::BufWriter::new(io::stdout().lock());
    for line in input.lines() {
        let request: Value = milvus_lite_core::json_text::value_str(&line?)?;
        serde_json::to_writer(&mut output, &answer(&request)?)?;
        writeln!(output)?;
    }
    output.flush()?;
    Ok(())
}
