#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use space_manager::{DEFAULT_SKIP, Progress, ScanOptions, ScanResult, human, scan};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

const MAX_CHILDREN_SHOWN: usize = 300;

#[derive(Clone)]
struct DeleteReq {
    path: PathBuf,
    size: u64,
    /// tree node index for folders, None for single files
    node: Option<usize>,
}

enum DeleteMode {
    Trash,
    Permanent,
}

fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elev = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elev as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        );
        CloseHandle(token);
        ok != 0 && elev.TokenIsElevated != 0
    }
}

/// Start an elevated copy of this exe (UAC prompt). True if it launched.
fn relaunch_elevated(path: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let Ok(exe) = std::env::current_exe() else { return false };
    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(Some(0)).collect() };
    let verb = wide("runas".as_ref());
    let file = wide(exe.as_os_str());
    // a trailing backslash before the closing quote must be doubled ("C:\" -> "C:\\")
    let mut arg = path.to_string();
    if arg.ends_with('\\') {
        arg.push('\\');
    }
    let params = wide(format!("\"{}\"", arg).as_ref());
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

fn clear_readonly(path: &std::path::Path) {
    for e in jwalk::WalkDir::new(path).skip_hidden(false).into_iter().flatten() {
        let p = e.path();
        if let Ok(m) = std::fs::symlink_metadata(&p) {
            let mut perm = m.permissions();
            if perm.readonly() {
                perm.set_readonly(false);
                let _ = std::fs::set_permissions(&p, perm);
            }
        }
    }
}

fn hint(err: &str) -> &'static str {
    if err.contains("os error 32") {
        "

A program is using this file. Close it and try again."
    } else if err.contains("os error 5") || err.contains("ccess is denied") {
        "

Access denied: the item is protected, owned by another account, or needs administrator rights.          Close programs using it, or run the app as administrator."
    } else {
        ""
    }
}

/// Why a path must never be deleted from this app, if any.
fn protected_reason(path: &std::path::Path, scan_root: &std::path::Path) -> Option<&'static str> {
    let norm = |p: &std::path::Path| p.display().to_string().trim_end_matches('\\').to_lowercase();
    let target = norm(path);
    if path.parent().is_none() || path.components().count() <= 1 {
        return Some("drive root");
    }
    if target == norm(scan_root) {
        return Some("scan root");
    }
    for var in ["SystemRoot", "WINDIR", "ProgramFiles", "ProgramFiles(x86)", "ProgramData", "USERPROFILE", "PUBLIC"] {
        if let Some(v) = std::env::var_os(var) {
            if norm(std::path::Path::new(&v)) == target {
                return Some("protected system folder");
            }
        }
    }
    if path.parent().is_some_and(|pp| norm(pp).ends_with(':')) && target.ends_with("\\users") {
        return Some("protected system folder");
    }
    None
}

struct App {
    path: String,
    progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<ScanResult>>,
    result: Option<ScanResult>,
    pending: Option<DeleteReq>,
    deleting: Option<(DeleteReq, mpsc::Receiver<Result<(), String>>)>,
    message: Option<String>,
    elevated: bool,
}

impl App {
    fn new() -> Self {
        Self {
            path: std::env::args().nth(1).unwrap_or_else(|| "C:\\".to_string()),
            progress: Arc::new(Progress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
            result: None,
            pending: None,
            deleting: None,
            message: None,
            elevated: is_elevated(),
        }
    }

    fn scanning(&self) -> bool {
        self.rx.is_some()
    }

    fn start(&mut self, ctx: &egui::Context) {
        self.progress = Arc::new(Progress::default());
        self.cancel = Arc::new(AtomicBool::new(false));
        self.result = None;
        self.message = None;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);

        let opts = ScanOptions {
            root: PathBuf::from(&self.path),
            skip: DEFAULT_SKIP.iter().map(|s| s.to_string()).collect(),
            top: 100,
        };
        let progress = Arc::clone(&self.progress);
        let cancel = Arc::clone(&self.cancel);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let res = scan(&opts, &progress, &cancel);
            let _ = tx.send(res);
            ctx.request_repaint();
        });
    }

    fn run_delete(&mut self, req: DeleteReq, mode: DeleteMode, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let path = req.path.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let out = match mode {
                DeleteMode::Trash => trash::delete(&path).map_err(|e| e.to_string()),
                DeleteMode::Permanent => {
                    let del = |p: &std::path::Path| if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
                    let mut r = del(&path);
                    if matches!(&r, Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied) {
                        // read-only attribute blocks delete on Windows: clear it and retry once
                        clear_readonly(&path);
                        r = del(&path);
                    }
                    r.map_err(|e| e.to_string())
                }
            };
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        self.deleting = Some((req, rx));
    }

    fn poll(&mut self) {
        if let Some((req, rx)) = &self.deleting {
            if let Ok(out) = rx.try_recv() {
                let req = req.clone();
                self.deleting = None;
                match out {
                    Ok(()) => {
                        if let Some(res) = &mut self.result {
                            res.remove_path(&req.path, req.size, req.node);
                        }
                        self.message = Some(format!("Deleted {} ({})", req.path.display(), human(req.size)));
                    }
                    Err(e) => self.message = Some(format!("Delete failed: {}
{}{}", req.path.display(), e, hint(&e))),
                }
            }
        }
        if let Some(rx) = &self.rx {
            if let Ok(res) = rx.try_recv() {
                self.result = Some(res);
                self.rx = None;
            }
        }
    }
}

fn reveal(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

fn reveal_file(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer").arg("/select,").arg(path).spawn();
}

fn draw_node(ui: &mut egui::Ui, res: &ScanResult, idx: usize, parent_size: u64, act: &mut Option<DeleteReq>) {
    let n = &res.nodes[idx];
    let pct = if parent_size > 0 { n.size as f64 * 100.0 / parent_size as f64 } else { 100.0 };
    let text = egui::RichText::new(format!("{:>10}  {:>5.1}%  {}", human(n.size), pct, n.name)).monospace();

    let resp = if n.children.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);
            ui.label(text)
        })
        .inner
    } else {
        egui::CollapsingHeader::new(text)
            .id_salt(idx)
            .show(ui, |ui| {
                for &c in n.children.iter().take(MAX_CHILDREN_SHOWN) {
                    draw_node(ui, res, c, n.size, act);
                }
                if n.children.len() > MAX_CHILDREN_SHOWN {
                    ui.label(format!("... {} more folders", n.children.len() - MAX_CHILDREN_SHOWN));
                }
                let own = res.own_files_size(idx);
                if own > 0 {
                    ui.label(
                        egui::RichText::new(format!("{:>10}  (files in this folder: {})", human(own), n.files))
                            .monospace()
                            .weak(),
                    );
                }
            })
            .header_response
    };
    resp.context_menu(|ui| {
        if ui.button("Open in Explorer").clicked() {
            reveal(&n.path);
            ui.close();
        }
        if ui.button("Copy path").clicked() {
            ui.ctx().copy_text(n.path.display().to_string());
            ui.close();
        }
        if idx != 0 && ui.button("Delete...").clicked() {
            *act = Some(DeleteReq { path: n.path.clone(), size: n.size, node: Some(idx) });
            ui.close();
        }
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll();
        if self.scanning() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Path:");
                ui.add_enabled(
                    !self.scanning(),
                    egui::TextEdit::singleline(&mut self.path).desired_width(400.0),
                );
                if ui.add_enabled(!self.scanning(), egui::Button::new("Browse...")).clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.path = p.display().to_string();
                    }
                }
                if self.elevated {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, "Administrator");
                } else if ui
                    .add_enabled(!self.scanning() && self.deleting.is_none(), egui::Button::new("Restart as administrator"))
                    .on_hover_text("Needed to delete files owned by SYSTEM or other accounts")
                    .clicked()
                {
                    if relaunch_elevated(&self.path) {
                        std::process::exit(0);
                    } else {
                        self.message = Some("Elevation was cancelled or failed.".to_string());
                    }
                }
                if self.scanning() {
                    if ui.button("Cancel").clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                } else if ui.button("Scan").clicked() {
                    self.start(&ctx);
                }
            });
            ui.add_space(2.0);
            if self.scanning() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "Scanning... {} files, {}",
                        self.progress.files.load(Ordering::Relaxed),
                        human(self.progress.bytes.load(Ordering::Relaxed)),
                    ));
                });
            } else if let Some(r) = &self.result {
                let mut s = format!(
                    "{} files, {} in {:.1?}",
                    r.files,
                    human(r.nodes.first().map_or(0, |n| n.size)),
                    r.elapsed
                );
                if r.errors > 0 {
                    s += &format!(" - {} unreadable entries skipped", r.errors);
                }
                if r.cancelled {
                    s += " - cancelled (partial)";
                }
                ui.label(s);
            }
            ui.add_space(2.0);
        });

        let mut act: Option<DeleteReq> = None;
        if let Some(res) = &self.result {
            egui::Panel::right("largest").default_size(420.0).show(ui, |ui| {
                ui.heading("Largest files");
                egui::ScrollArea::vertical().id_salt("files").show(ui, |ui| {
                    for (size, path) in &res.largest {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let resp = ui
                            .label(egui::RichText::new(format!("{:>10}  {}", human(*size), name)).monospace())
                            .on_hover_text(path.display().to_string());
                        resp.context_menu(|ui| {
                            if ui.button("Show in Explorer").clicked() {
                                reveal_file(path);
                                ui.close();
                            }
                            if ui.button("Copy path").clicked() {
                                ui.ctx().copy_text(path.display().to_string());
                                ui.close();
                            }
                            if ui.button("Delete...").clicked() {
                                act = Some(DeleteReq { path: path.clone(), size: *size, node: None });
                                ui.close();
                            }
                        });
                    }
                });
            });

            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("Folders");
                egui::ScrollArea::both().id_salt("tree").show(ui, |ui| {
                    if !res.nodes.is_empty() {
                        draw_node(ui, res, 0, res.nodes[0].size, &mut act);
                    }
                });
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                if !self.scanning() {
                    ui.label("Pick a folder or drive and press Scan.");
                }
            });
        }

        if act.is_some() && self.deleting.is_none() {
            self.pending = act;
        }

        // status of last delete
        if let Some(msg) = self.message.clone() {
            let mut close = false;
            egui::Window::new("Info").collapsible(false).resizable(false).show(&ctx, |ui| {
                ui.label(&msg);
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
            if close {
                self.message = None;
            }
        }

        if self.deleting.is_some() {
            egui::Window::new("Deleting").collapsible(false).resizable(false).show(&ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Deleting...");
                });
            });
        }

        // confirmation dialog
        if let Some(req) = self.pending.clone() {
            let root = self.result.as_ref().map(|r| r.nodes[0].path.clone()).unwrap_or_default();
            let blocked = protected_reason(&req.path, &root);
            let mut choice: Option<Option<DeleteMode>> = None; // Some(None) = cancel
            egui::Window::new("Confirm delete").collapsible(false).resizable(false).show(&ctx, |ui| {
                ui.label(req.path.display().to_string());
                ui.label(format!("Size: {}", human(req.size)));
                ui.add_space(6.0);
                if let Some(why) = blocked {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("Refusing to delete: {why}."));
                    if ui.button("Close").clicked() {
                        choice = Some(None);
                    }
                } else {
                    ui.label("Moves to the Recycle Bin (can be restored).");
                    ui.horizontal(|ui| {
                        if ui.button("Move to Recycle Bin").clicked() {
                            choice = Some(Some(DeleteMode::Trash));
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(None);
                        }
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.colored_label(egui::Color32::LIGHT_RED, "Permanent delete cannot be undone.");
                    if ui.button("Delete permanently").clicked() {
                        choice = Some(Some(DeleteMode::Permanent));
                    }
                }
            });
            if let Some(c) = choice {
                self.pending = None;
                if let Some(mode) = c {
                    self.run_delete(req, mode, &ctx);
                }
            }
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 700.0]).with_title("Space Manager").with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).expect("icon")),
        ..Default::default()
    };
    eframe::run_native("Space Manager", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}
