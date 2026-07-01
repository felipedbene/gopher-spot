//! gopher-spot CLI. Two shapes share one binary:
//!   gopher-spot root
//!       Print the static root menu (.gph). Baked to /srv/index.gph at build.
//!   gopher-spot dcgi $search $arguments $host $port $traversal $selector
//!       The dcgi entry geomyidae calls for /spot/* selectors; prints a gophermap.

use std::process::ExitCode;

use gopher_spot::{dcgi, menu};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("root") => {
            print!("{}", menu::root_gph());
            ExitCode::SUCCESS
        }
        Some("dcgi") => {
            let a = dcgi::DcgiArgs::from_argv(&args[2..]);
            print!("{}", dcgi::route(&a));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: gopher-spot <root|dcgi> [args...]");
            ExitCode::from(2)
        }
    }
}
