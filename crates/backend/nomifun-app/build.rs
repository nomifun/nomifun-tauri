fn main() {
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let ts = std::env::var("SOURCE_DATE_EPOCH").unwrap_or_else(|_| {
        if std::env::var("PROFILE").as_deref() == Ok("release") {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock must be after the Unix epoch")
                .as_secs()
                .to_string()
        } else {
            "0".to_owned()
        }
    });
    println!("cargo:rustc-env=BUILD_TIME={ts}");
    println!("cargo:rerun-if-changed=build.rs");
}
