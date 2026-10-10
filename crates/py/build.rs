fn reject(reason: &str) -> ! {
    eprintln!("milvus-lite-rust build rejected: {reason}");
    std::process::exit(1)
}

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    // build.rs 在宿主运行，拒绝必须按目标判断，才能覆盖交叉编译与 pip 源码回退。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        reject("Windows is not supported in this release; use Linux or macOS");
    }
    let config = pyo3_build_config::get();
    if config.implementation() != pyo3_build_config::PythonImplementation::CPython {
        reject("only GIL-enabled CPython is supported");
    }
    if config.is_free_threaded() {
        reject("free-threaded Python is not supported; use GIL-enabled CPython");
    }
}
