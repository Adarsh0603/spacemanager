use clap::Parser;
use space_manager::{DEFAULT_SKIP, Progress, ScanOptions, human, scan};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Fast disk usage scanner.
#[derive(Parser)]
struct Args {
    /// Folder or drive to scan
    #[arg(default_value = "C:\\")]
    path: PathBuf,

    /// Number of entries to show
    #[arg(short, long, default_value_t = 20)]
    top: usize,

    /// Directory names to skip (repeatable)
    #[arg(long)]
    skip: Vec<String>,
}

fn main() {
    let args = Args::parse();
    let skip = if args.skip.is_empty() {
        DEFAULT_SKIP.iter().map(|s| s.to_string()).collect()
    } else {
        args.skip
    };
    let opts = ScanOptions { root: args.path.clone(), skip, top: args.top };
    let res = scan(&opts, &Arc::new(Progress::default()), &AtomicBool::new(false));

    println!("\nTop folders in {}:", args.path.display());
    let mut rows: Vec<(String, u64)> = res.nodes[0]
        .children
        .iter()
        .map(|&c| (res.nodes[c].name.clone(), res.nodes[c].size))
        .collect();
    let own = res.own_files_size(0);
    if own > 0 {
        rows.push(("(files in root)".to_string(), own));
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, size) in rows.iter().take(args.top) {
        println!("{:>10}  {}", human(*size), name);
    }
    println!("\nLargest files:");
    for (size, path) in &res.largest {
        println!("{:>10}  {}", human(*size), path.display());
    }
    println!("\nScanned {} files in {:.1?}", res.files, res.elapsed);
    if res.errors > 0 {
        println!("Skipped {} unreadable entries (permissions/locks)", res.errors);
    }
}
