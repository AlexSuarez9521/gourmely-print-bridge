// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = print_bridge_lib::acl_repair::parse_startup_action(&args)
        .and_then(print_bridge_lib::acl_repair::execute_startup_action);
    match result {
        Ok(false) => print_bridge_lib::run(),
        Ok(true) => {}
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
