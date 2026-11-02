//! Optional GNU workaround for non-ASCII toolchain paths. The caller explicitly
//! supplies an equivalent ASCII sysroot through JIKS_RUST_SYSROOT.
fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let Some(compiler) = arguments.next() else {
        std::process::exit(2);
    };
    let mut command = std::process::Command::new(compiler);
    command.args(arguments);
    if let Some(sysroot) = std::env::var_os("JIKS_RUST_SYSROOT") {
        command.arg("--sysroot").arg(sysroot);
    }
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("Cannot start Rust compiler: {error}");
            std::process::exit(1);
        }
    }
}
