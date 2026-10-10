use vergen_gix::{Build, Cargo, Emitter, Gix, Rustc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build = Build::builder().build_timestamp(true).build();
    let cargo = Cargo::builder().target_triple(true).build();
    let git = Gix::builder().sha(true).dirty(true).build();
    let rustc = Rustc::builder()
        .semver(true)
        .channel(true)
        .host_triple(true)
        .llvm_version(true)
        .build();

    Emitter::default()
        .add_instructions(&build)?
        .add_instructions(&cargo)?
        .add_instructions(&git)?
        .add_instructions(&rustc)?
        .emit()?;

    Ok(())
}
