//! Step 7 gRPC 生成（spec step7 §3 D12）：tonic-prost-build 编译
//! proto/wiktor.proto 到 OUT_DIR（tonic-build 0.14 的 prost codegen 已迁移到
//! tonic-prost-build）。需要 protoc（本机 36.2；CI 若只 push 不构建可不装）。
//! Step 7 gRPC generation (spec step7 §3 D12): tonic-prost-build compiles
//! proto/wiktor.proto into OUT_DIR (tonic-build 0.14 moved prost codegen to the
//! tonic-prost-build crate). Requires protoc (local 36.2; a push-only CI that
//! never builds may skip installing it).
fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=proto/wiktor.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/wiktor.proto"], &["proto"])
}
