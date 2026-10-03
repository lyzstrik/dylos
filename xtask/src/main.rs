#![forbid(unsafe_code)]

use std::env;

fn main() {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("e2e") => {
            eprintln!("xtask: e2e is not implemented yet");
        }
        Some(subcommand) => {
            eprintln!("xtask: unknown subcommand '{subcommand}'");
            std::process::exit(1);
        }
        None => {
            eprintln!("xtask: missing subcommand");
            std::process::exit(1);
        }
    }
}
