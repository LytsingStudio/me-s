#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(arguments.as_slice(), [argument] if argument == "version" || argument == "--version") {
        println!("me-client {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    me_client_lib::run();
}
