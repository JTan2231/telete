fn main() -> std::process::ExitCode {
    cell_install::simple::main_with_deployment(
        &bazaar::installation::specification(),
        env!("CARGO_PKG_VERSION"),
        bazaar::installation::deploy,
    )
}
