fn main() {
    let schema = "data/io.github.oxidance.gschema.xml";
    println!("cargo:rerun-if-changed={schema}");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::copy(schema, output.join("io.github.oxidance.gschema.xml")).unwrap();
    let status = std::process::Command::new("glib-compile-schemas")
        .arg("--strict").arg(&output).status().expect("glib-compile-schemas is required");
    assert!(status.success(), "GSettings schema compilation failed");
    println!("cargo:rerun-if-changed=data/oxidance.gresource.xml");
    println!("cargo:rerun-if-changed=data/icons/scalable/apps/io.github.oxidance.Oxidance.svg");
    let status = std::process::Command::new("glib-compile-resources")
        .arg("data/oxidance.gresource.xml").arg("--sourcedir=data")
        .arg("--target").arg(output.join("oxidance.gresource"))
        .status().expect("glib-compile-resources is required to embed the app icon");
    assert!(status.success(), "Could not compile the app icon resource");
}
