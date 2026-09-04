mod ascii;
mod auth;
mod catalog;
mod cli;
mod convert;
mod docmeta;
mod encoding;
mod http;
mod json;
mod llms;
mod mcp;
mod mime;
mod page;
mod plugin;
mod render;
mod scanner;
mod search;
mod sitemap;
mod template;

use cli::ParseOutcome;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(ParseOutcome::Run(cfg)) => {
            if let Err(e) = http::serve(cfg) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Ok(ParseOutcome::Help) => print!("{}", cli::help()),
        Ok(ParseOutcome::Version) => println!("serve-md {}", env!("CARGO_PKG_VERSION")),
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!();
            eprint!("{}", cli::help());
            std::process::exit(2);
        }
    }
}
