fn main() {
    if let Some(snapshot) = iatreion_api::requested_on_demand_snapshot_json(
        "chancery",
        env!("CARGO_PKG_VERSION"),
        "chancery/catalog",
        "chancery.directory.discover",
    ) {
        chancery_usage::observe("chancery", "status-snapshot");
        println!("{snapshot}");
        return;
    }
    let exit_code = chancery::run_cli();
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}
