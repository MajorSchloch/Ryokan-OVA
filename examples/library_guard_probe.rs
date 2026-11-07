//! Test the same lifetime lock and recovery check used by application startup.
use ryokan::services::library_guard::LibraryLease;
use std::io::{BufRead, Write};

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("Usage: library_guard_probe DB check|hold".into());
    }
    let lease = LibraryLease::acquire(std::path::Path::new(&args[0]))?;
    lease.require_clean()?;
    if args[1] == "hold" {
        println!("ready");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let _ = std::io::stdin().lock().lines().next();
    } else if args[1] != "check" {
        return Err("Unknown probe mode".into());
    }
    Ok(())
}
