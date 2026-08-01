#![doc = "Embeds the Windows executable icon during GUI builds."]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=unclean.rc");
    println!("cargo:rerun-if-changed=../../assets/unclean.ico");

    embed_resource::compile_for("unclean.rc", ["unclean-gui"], embed_resource::NONE)
        .manifest_required()?;

    Ok(())
}
