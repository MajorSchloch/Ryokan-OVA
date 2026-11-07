//! Run with: cargo run --example migration_preview -- SNAPSHOT.json MANIFEST.json
//! Writes a review document to stdout. There is deliberately no --apply option.
use ryokan::services::migration_preview::{Manifest, build};

fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("Usage: migration_preview SNAPSHOT.json MANIFEST.json (read-only)".into());
    }
    let snapshot = std::fs::read(&args[0]).map_err(|e| e.to_string())?;
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(&args[1]).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let preview = build(&snapshot, &manifest)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&preview).map_err(|e| e.to_string())?
    );
    Ok(())
}
