use std::io::BufRead;
fn main()->Result<(),Box<dyn std::error::Error>> {
 let args:Vec<String>=std::env::args().collect();
 if args.len()!=4 {return Err("input.jsonl new-output-dir masked-bool required".into());}
 let path=std::path::Path::new(&args[2]);
 let work=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).ok_or("work root missing")?.canonicalize()?;
 let parent=path.parent().ok_or("parent missing")?.canonicalize()?;
 if path.exists() || path.is_symlink() || !parent.starts_with(&work) || parent.starts_with(work.join("src")) {return Err("output must be a new path under the isolated work root, outside its source tree".into());}
 std::fs::create_dir(path)?;
 let raw:Vec<Vec<u8>>=std::io::BufReader::new(std::fs::File::open(&args[1])?).lines().map(|line|line.map(String::into_bytes)).collect::<Result<_,_>>()?;
 if raw.len()!=5000 {return Err("5000 rows required".into());}
 let codec=milvus_lite_core::storage::arrow_data::json_codec_diagnostic(&raw)?;
 let stages=milvus_lite_core::collection::json_write_diagnostic(raw,path,args[3].parse()? )?;
 println!("{}",serde_json::json!({"codec":codec,"stages":stages}));
 Ok(())
}
