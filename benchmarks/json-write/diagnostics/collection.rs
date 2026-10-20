
/// 在隔离源码副本中调用原组件；只用于调查，不参与发行构建。
pub fn json_write_diagnostic(raw: Vec<Vec<u8>>, out: &std::path::Path, masked: bool) -> std::result::Result<Value, Box<dyn std::error::Error>> {
    use std::time::Instant;
    use crate::storage::{arrow_data,wal::{WalWriter,WalKind,read_prefix}};
    fn rss() -> std::result::Result<u64, Box<dyn std::error::Error>> {
        let output = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output()?;
        if !output.status.success() { return Err("ps failed".into()); }
        Ok(String::from_utf8(output.stdout)?.trim().parse::<u64>()? * 1024)
    }
    let n=raw.len();
    let mut field=crate::schema::FieldSchema::new("j",crate::schema::DataType::Json);field.nullable=masked;
    let schema=crate::schema::CollectionSchema { fields:vec![field.clone()],enable_dynamic_field:false,properties:Vec::new() };
    let seqs:Vec<u64>=(1..=n as u64).collect();
    let mut samples=Vec::new();
    for sample in 0..6 {
        let dir=out.join(format!("sample-{sample}"));std::fs::create_dir(&dir)?;
        let mut times=serde_json::Map::new();let mut memory=serde_json::Map::new();
        memory.insert("start".into(),rss()?.into());
        let t=Instant::now();
        let mut input=InputValues::Json(raw.clone());
        if masked { input=InputValues::Nullable { values: Box::new(input), validity:crate::data::Validity::from_bools(&(0..n).map(|i|i%2==0).collect::<Vec<_>>()) }; }
        times.insert("request_clone_ms".into(),t.elapsed().as_secs_f64().mul_add(1000.0,0.0).into());
        memory.insert("request".into(),rss()?.into());
        let t=Instant::now();
        for row in 0..n {
            if masked { parse_json("j", &raw[row])?; }
            let (value,index)=resolved_input(&field,Some(&input),row)?.ok_or("missing")?;
            check_value(&field,&value,index)?;
        }
        times.insert("validate_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        let t=Instant::now();let mut columns=vec![convert_optional(&field,Some(&input),n)?];
        times.insert("convert_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        memory.insert("converted".into(),rss()?.into());
        let t=Instant::now();arrow_data::normalize_data_json(&schema,&mut columns)?;
        times.insert("normalize_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        memory.insert("normalized".into(),rss()?.into());
        let t=Instant::now();let batch=arrow_data::wal_data_batch(&schema,&seqs,"_default",&columns,None)?;
        times.insert("arrow_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        memory.insert("arrow".into(),rss()?.into());
        let bytes=batch.get_array_memory_size();
        let t=Instant::now();let mut writer=WalWriter::create(&dir,WalKind::Data,1,&batch.schema())?;
        times.insert("wal_create_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        let t=Instant::now();writer.append(&batch)?;
        times.insert("wal_append_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        memory.insert("wal_appended".into(),rss()?.into());
        let t=Instant::now();writer.into_file().sync_all()?;
        times.insert("wal_sync_ms".into(),(t.elapsed().as_secs_f64()*1000.0).into());
        let recovered=read_prefix(&dir.join(WalKind::Data.file_name(1)))?;
        if recovered.batches.len()!=1 {return Err("WAL batch count mismatch".into());}
        let decoded=arrow_data::decode_wal_data_batch(&schema,&recovered.batches[0])?;
        for row in 0..n {
            let actual=match decoded.columns[0].values() { ColumnData::Json(v)=>&v[row],_=>return Err("JSON column missing".into()) };
            let expected=if masked && row%2!=0 {Value::Null} else {serde_json::from_slice(&raw[row])?};
            let actual_value:Value=serde_json::from_slice(actual)?;
            if actual_value!=expected || decoded.columns[0].is_null(row)!=(masked && row%2!=0) {return Err("WAL readback mismatch".into());}
        }
        if sample!=0 {samples.push(serde_json::json!({"ms":times,"rss_bytes":memory,"arrow_bytes":bytes}));}
    }
    Ok(serde_json::json!({"rows":n,"masked":masked,"samples":samples,"input_bytes":raw.iter().map(|x|x.len()).sum::<usize>(),"rss_scope":"standalone Rust component including retained input; snapshots outside timing; not allocator or server peak"}))
}
