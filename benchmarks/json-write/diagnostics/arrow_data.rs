
/// 独立计时包含计时器开销；墙钟与分项分别保留，不用于替代端到端测量。
pub fn json_codec_diagnostic(raw: &[Vec<u8>]) -> Result<serde_json::Value, ArrowDataError> {
    use std::time::Instant;
    let mut samples=Vec::new();
    for sample in 0..6 {
        let wall=Instant::now();let (mut parse,mut finite,mut dump)=(0.0,0.0,0.0);let mut bytes=0;
        for text in raw {
            let t=Instant::now();let v:Value=serde_json::from_slice(text).map_err(|source|ArrowDataError::JsonText {name:"j".into(),source})?;parse+=t.elapsed().as_secs_f64()*1000.0;
            let t=Instant::now();check_finite_json_numbers("j",&v)?;finite+=t.elapsed().as_secs_f64()*1000.0;
            let t=Instant::now();let output=pyjson::dumps_unicode(&v);dump+=t.elapsed().as_secs_f64()*1000.0;bytes+=output.len();std::hint::black_box(output);
        }
        if sample!=0 {samples.push(serde_json::json!({"wall_ms":wall.elapsed().as_secs_f64()*1000.0,"parse_ms":parse,"finite_ms":finite,"dump_ms":dump,"output_bytes":bytes}));}
    }
    Ok(serde_json::json!({"rows":raw.len(),"samples":samples}))
}
