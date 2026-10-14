fn main() -> Result<(), Box<dyn std::error::Error>> {
    // proto 位于仓库根 proto/（任务卡 T-001 步骤 3：用 vendored protoc，避免本机安装）
    let proto = "../../proto/rd.proto";
    println!("cargo:rerun-if-changed={proto}");

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // prost-build 通过 PROTOC 环境变量定位编译器
    std::env::set_var("PROTOC", protoc);
    prost_build::compile_protos(&[proto], &["../../proto"])?;
    Ok(())
}
