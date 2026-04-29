fn main() {
    slint_build::compile("ui/main.slint").expect("Slint compilation failed");

    #[cfg(windows)]
    {
        // Windows GUI subsystem — no console window on launch
        println!("cargo:rustc-link-arg-bins=/SUBSYSTEM:WINDOWS");
        println!("cargo:rustc-link-arg-bins=/ENTRY:mainCRTStartup");
    }
}
