use jwalk::WalkDirGeneric;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

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

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.1} {}", v, UNITS[i])
}

pub const DEFAULT_SKIP: [&str; 3] = ["$Recycle.Bin", "System Volume Information", "WinSxS"];

pub struct ScanOptions {
    pub root: PathBuf,
    pub skip: Vec<String>,
    /// number of largest files to keep
    pub top: usize,
}

/// Live counters, readable from another thread while a scan runs.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
}

pub struct Node {
    pub name: String,
    pub path: PathBuf,
    /// total size of everything below (files directly inside + subfolders)
    pub size: u64,
    /// number of files directly inside this folder
    pub files: u64,
    pub parent: Option<usize>,
    /// sorted by size, descending, once the scan finishes
    pub children: Vec<usize>,
}

pub struct ScanResult {
    /// nodes[0] is the root
    pub nodes: Vec<Node>,
    /// largest files, descending
    pub largest: Vec<(u64, PathBuf)>,
    pub files: u64,
    pub errors: u64,
    pub elapsed: Duration,
    pub cancelled: bool,
}

impl ScanResult {
    /// Size of files sitting directly in a folder (not in any subfolder).
    pub fn own_files_size(&self, idx: usize) -> u64 {
        let n = &self.nodes[idx];
        n.size - n.children.iter().map(|&c| self.nodes[c].size).sum::<u64>()
    }

    /// Update the result after `path` (a folder node or a single file) was deleted.
    /// `size` is the size shown for it; for a folder node it is taken from the node.
    pub fn remove_path(&mut self, path: &std::path::Path, size: u64, node: Option<usize>) {
        let (size, removed_files, start) = match node {
            Some(idx) if idx != 0 => {
                let size = self.nodes[idx].size;
                let mut files = 0;
                let mut stack = vec![idx];
                while let Some(i) = stack.pop() {
                    files += self.nodes[i].files;
                    stack.extend(self.nodes[i].children.iter().copied());
                }
                let parent = self.nodes[idx].parent;
                if let Some(p) = parent {
                    self.nodes[p].children.retain(|&c| c != idx);
                }
                (size, files, parent)
            }
            Some(_) => return, // never remove the scan root
            None => {
                let start = path
                    .parent()
                    .and_then(|pp| self.nodes.iter().position(|n| n.path == pp));
                if let Some(i) = start {
                    self.nodes[i].files = self.nodes[i].files.saturating_sub(1);
                }
                (size, 1, start)
            }
        };
        let mut cur = start;
        while let Some(i) = cur {
            self.nodes[i].size = self.nodes[i].size.saturating_sub(size);
            cur = self.nodes[i].parent;
        }
        self.files = self.files.saturating_sub(removed_files);
        self.largest.retain(|(_, p)| !p.starts_with(path));
    }
}

pub fn scan(opts: &ScanOptions, progress: &Arc<Progress>, cancel: &AtomicBool) -> ScanResult {
    let start = Instant::now();
    let skip = opts.skip.clone();
    let cb_progress = Arc::clone(progress);

    let walker = WalkDirGeneric::<((), Option<FileInfo>)>::new(&opts.root)
        .skip_hidden(false)
        .follow_links(false)
        .process_read_dir(move |_, _, _, children| {
            children.retain(|c| match c {
                Ok(e) => !(e.file_type().is_dir()
                    && skip.iter().any(|s| e.file_name().to_string_lossy().eq_ignore_ascii_case(s))),
                Err(_) => {
                    cb_progress.errors.fetch_add(1, Ordering::Relaxed);
                    false
                }
            });
            // stat files here: runs on the worker pool, not the consumer thread
            for child in children.iter_mut().flatten() {
                if child.file_type().is_file() {
                    let info = disk_info(&child.path());
                    match info {
                        Some(i) => child.client_state = Some(i),
                        None => match child.metadata() {
                            Ok(m) => child.client_state = Some((m.len(), None)),
                            Err(_) => {
                                cb_progress.errors.fetch_add(1, Ordering::Relaxed);
                            }
                        },
                    }
                }
            }
        });

    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<(u64, PathBuf)>> = BinaryHeap::new();
    let mut seen_links: HashSet<(u64, u128)> = HashSet::new();
    let mut count: u64 = 0;
    let mut cancelled = false;

    for entry in walker.into_iter() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_dir() {
            let path = entry.path();
            let parent = path.parent().and_then(|p| index.get(p)).copied();
            let name = if parent.is_none() {
                path.display().to_string()
            } else {
                entry.file_name().to_string_lossy().into_owned()
            };
            let idx = nodes.len();
            nodes.push(Node { name, path: path.clone(), size: 0, files: 0, parent, children: Vec::new() });
            if let Some(p) = parent {
                nodes[p].children.push(idx);
            }
            index.insert(path, idx);
            continue;
        }
        if !ft.is_file() {
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
        progress.files.store(count, Ordering::Relaxed);
        progress.bytes.fetch_add(size, Ordering::Relaxed);

        let path = entry.path();
        let mut cur = path.parent().and_then(|p| index.get(p)).copied();
        let mut first = true;
        while let Some(i) = cur {
            nodes[i].size += size;
            if first {
                nodes[i].files += 1;
                first = false;
            }
            cur = nodes[i].parent;
        }

        if opts.top > 0 {
            if heap.len() < opts.top {
                heap.push(Reverse((size, path)));
            } else if let Some(Reverse((min, _))) = heap.peek() {
                if size > *min {
                    heap.pop();
                    heap.push(Reverse((size, path)));
                }
            }
        }
    }

    // sort children by size, descending
    for i in 0..nodes.len() {
        let mut kids = std::mem::take(&mut nodes[i].children);
        kids.sort_by(|&a, &b| nodes[b].size.cmp(&nodes[a].size));
        nodes[i].children = kids;
    }

    ScanResult {
        nodes,
        largest: heap.into_sorted_vec().into_iter().map(|Reverse(x)| x).collect(),
        files: count,
        errors: progress.errors.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
        cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn remove_path_updates_sizes() {
        let dir = std::env::temp_dir().join(format!("sm_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/big.bin"), vec![1u8; 100_000]).unwrap();
        fs::write(dir.join("a/small.txt"), b"hi").unwrap();

        let opts = ScanOptions { root: dir.clone(), skip: vec![], top: 10 };
        let mut res = scan(&opts, &Arc::new(Progress::default()), &AtomicBool::new(false));
        let total = res.nodes[0].size;
        assert!(total >= 100_002); // size on disk >= logical size
        assert_eq!(res.files, 2);

        // delete folder a/b
        let b = res.nodes.iter().position(|n| n.name == "b").unwrap();
        let bp = res.nodes[b].path.clone();
        let b_size = res.nodes[b].size;
        res.remove_path(&bp, 0, Some(b));
        assert_eq!(res.nodes[0].size, total - b_size);
        assert_eq!(res.files, 1);
        assert!(res.largest.iter().all(|(_, p)| !p.starts_with(&bp)));

        // delete single file
        let f = dir.join("a").join("small.txt");
        let remaining = res.nodes[0].size;
        res.remove_path(&f, remaining, None);
        assert_eq!(res.nodes[0].size, 0);
        assert_eq!(res.files, 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
