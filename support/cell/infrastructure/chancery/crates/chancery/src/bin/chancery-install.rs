fn main() -> std::process::ExitCode {
    cell_install::simple::main(
        &chancery::installation::specification(),
        env!("CARGO_PKG_VERSION"),
    )
}
