use std::io::Result;

fn main() -> Result<()> {
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());

    let proto_dir = "../../proto";
    let proto_files = [
        "event_header.proto",
        "market_event.proto",
        "decision_event.proto",
        "execution_event.proto",
        "state_snapshot.proto",
        "policy_command.proto",
        "trade_skip_event.proto",
        "gap_event.proto",
    ];

    let proto_paths: Vec<String> = proto_files
        .iter()
        .map(|f| format!("{proto_dir}/{f}"))
        .collect();

    prost_build::Config::new().compile_protos(&proto_paths, &[proto_dir])?;

    Ok(())
}
