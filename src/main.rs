use clap::Parser;
use jwalk::WalkDirGeneric;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
    #[arg(long, default_values_t = ["$Recycle.Bin".to_string(), "System Volume Information".to_string(), "WinSxS".to_string()])]
    skip: Vec<String>,
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, UNITS[i])
}

fn main() {
    let args = Args::parse();
    let start = Instant::now();
    let root = args.path.clone();
    let skip = args.skip.clone();
    let errors = Arc::new(AtomicU64::new(0));
    let errors_cb = Arc::clone(&errors);

    let mut by_top: HashMap<String, u64> = HashMap::new();
    // min-heap: smallest of the kept top-N sits at the root
    let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
    let mut count: u64 = 0;

    let walker = WalkDirGeneric::<((), Option<u64>)>::new(&root)
        .skip_hidden(false)
        .follow_links(false)
        .process_read_dir(move |_, _, _, children| {
            // prune skipped directories before descending
            children.retain(|c| match c {
                Ok(e) => !(e.file_type().is_dir()
                    && skip.iter().any(|s| e.file_name().to_string_lossy().eq_ignore_ascii_case(s))),
                Err(_) => {
                    errors_cb.fetch_add(1, Ordering::Relaxed);
                    false
                }
            });
            // stat files here: this callback runs on the worker pool, not the consumer thread
            for child in children.iter_mut().flatten() {
                if child.file_type().is_file() {
                    match child.metadata() {
                        Ok(m) => child.client_state = Some(m.len()),
                        Err(_) => {
                            errors_cb.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

    for entry in walker.into_iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let size = entry.client_state.unwrap_or(0);
        count += 1;

        let rel = entry.path();
        let rel = rel.strip_prefix(&root).unwrap_or(&rel).to_path_buf();
        // files directly under root have a single component: the file itself
        let top = if rel.components().count() <= 1 {
            "(files in root)".to_string()
        } else {
            rel.components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        *by_top.entry(top).or_insert(0) += size;

        // keep only the largest N files in memory
        if args.top > 0 {
            if heap.len() < args.top {
                heap.push(Reverse((size, entry.path())));
            } else if let Some(Reverse((min, _))) = heap.peek() {
                if size > *min {
                    heap.pop();
                    heap.push(Reverse((size, entry.path())));
                }
            }
        }
    }
    let files = heap.into_sorted_vec(); // ascending of Reverse = descending by size

    let mut dirs: Vec<_> = by_top.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1));

    println!("\nTop folders in {}:", root.display());
    for (name, size) in dirs.iter().take(args.top) {
        println!("{:>10}  {}", human(*size), name);
    }
    println!("\nLargest files:");
    for Reverse((size, path)) in &files {
        println!("{:>10}  {}", human(*size), path.display());
    }
    println!("\nScanned {} files in {:.1?}", count, start.elapsed());
}
