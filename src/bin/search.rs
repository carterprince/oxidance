use std::{error::Error, process::ExitCode};

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("Usage: search <query>\nSearch YouTube Music's Songs category and print up to five songs as JSON.");
        return Ok(());
    }
    let query = args.join(" ");
    if query.trim().is_empty() { return Err("Usage: search <query>".into()); }
    println!("{}", serde_json::to_string_pretty(&oxidance::search(&query)?)?);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => { eprintln!("Error: {error}"); ExitCode::FAILURE }
    }
}
