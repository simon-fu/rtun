use protobuf_codegen::Customize;



fn main() -> Result<(), Box<dyn std::error::Error>> {

    println!("cargo:rerun-if-changed=proto/app.proto");

    let proto_files = [
        "proto/app.proto", 
    ];
    let include = "proto";


    // prost_build::compile_protos(&proto_files, &[include])?;

    let out_dir = std::env::var("OUT_DIR")?;
    let generated_with_pure_dir = format!("{}/generated_with_pure", out_dir);
    std::fs::create_dir_all(&generated_with_pure_dir)?;

    protobuf_codegen::Codegen::new()
    .pure()
    .customize(
        Customize::default()
        .gen_mod_rs(true)
        .tokio_bytes(true)
        .tokio_bytes_for_string(true)
        .generate_accessors(true)
        .generate_getter(true)
    )
    .out_dir(&generated_with_pure_dir)
    .inputs(&proto_files)
    .include(include)
    .run()?;

    Ok(())
}

