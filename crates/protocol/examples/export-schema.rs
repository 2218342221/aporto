fn main() {
    let schema = schemars::schema_for!(aporto_protocol::ProtocolSchema);
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}
