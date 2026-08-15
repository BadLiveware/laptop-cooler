fn main() {
    linker_be_nice();
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}

fn linker_be_nice() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 {
        if args[1] == "undefined-symbol" && args[2] == "_stack_start" {
            eprintln!("\nlinkall.x is required to define the ESP32 memory layout.\n");
        }
        std::process::exit(1);
    }

    println!(
        "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
        std::env::current_exe()
            .expect("build script path")
            .display()
    );
}
