use clap::Parser;
use jwalk::WalkDirGeneric;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
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

    /// Report logical file size instead of size on disk (faster, no hardlink dedup)
    #[arg(long)]
    apparent: bool,
}

/// (size in bytes, hardlink identity if the file has >1 link)
type FileInfo = (u64, Option<(u64, u128)>);

/// Size on disk (allocation size) and hardlink id via one cheap attribute-only handle.
fn disk_info(path: &std::path::Path) -> Option<FileInfo> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::*;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut std_info: FILE_STANDARD_INFO = std::mem::zeroed();
        let ok = GetFileInformationByHandleEx(
            h,
            FileStandardInfo,
            &mut std_info as *mut _ as *mut _,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        );
        if ok == 0 {
            CloseHandle(h);
            return None;
        }
        let mut id = None;
        if std_info.NumberOfLinks > 1 {
            let mut id_info: FILE_ID_INFO = std::mem::zeroed();
            let ok = GetFileInformationByHandleEx(
                h,
                FileIdInfo,
                &mut id_info as *mut _ as *mut _,
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            );
            if ok != 0 {
                id = Some((id_info.VolumeSerialNumber, u128::from_le_bytes(id_info.FileId.Identifier)));
            }
        }
        CloseHandle(h);
        Some((std_info.AllocationSize.max(0) as u64, id))
    }
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
    let apparent = args.apparent;
    let errors = Arc::new(AtomicU64::new(0));
    let errors_cb = Arc::clone(&errors);

    let mut by_top: HashMap<String, u64> = HashMap::new();
    // min-heap: smallest of the kept top-N sits at the root
    let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
    let mut count: u64 = 0;
    let mut seen_links: HashSet<(u64, u128)> = HashSet::new();

    let walker = WalkDirGeneric::<((), Option<FileInfo>)>::new(&root)
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
                    let info = if apparent { None } else { disk_info(&child.path()) };
                    match info {
                        Some(i) => child.client_state = Some(i),
                        None => match child.metadata() {
                            Ok(m) => child.client_state = Some((m.len(), None)),
                            Err(_) => {
                                errors_cb.fetch_add(1, Ordering::Relaxed);
                            }
                        },
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
        let (size, link_id) = entry.client_state.unwrap_or((0, None));
        if let Some(id) = link_id {
            // extra hardlink to a file already counted
            if !seen_links.insert(id) {
                continue;
            }
        }
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
