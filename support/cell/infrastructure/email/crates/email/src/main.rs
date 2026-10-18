fn main() {
    if let Some(snapshot) = iatreion_api::requested_on_demand_snapshot_json(
        "email",
        env!("CARGO_PKG_VERSION"),
        "email/transport",
        "email.message.send",
    ) {
        chancery_usage::observe("email", "status-snapshot");
        println!("{snapshot}");
        return;
    }
    email::main_entry();
}
