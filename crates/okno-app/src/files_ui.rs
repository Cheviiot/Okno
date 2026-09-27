//! The Files sheet of a session window.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okno_core::files::{FileError, Files};
use okno_proto::DirListing;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::{FileRow, Messages, SessionWindow, TransferRow};

enum Status {
    Running,
    Done,
    Failed(String),
}

struct Transfer {
    name: String,
    upload: bool,
    /// Local file of a download.
    local: Option<PathBuf>,
    done: u64,
    total: u64,
    status: Status,
}

/// Transfer progress shared with the tokio tasks; the UI copies it into
/// the model on a timer instead of receiving an event per chunk.
#[derive(Default)]
struct Progress {
    transfers: Mutex<Vec<Transfer>>,
    dirty: AtomicBool,
}

impl Progress {
    fn add(&self, name: String, local: Option<PathBuf>) -> usize {
        let mut list = self.transfers.lock().unwrap();
        list.push(Transfer { name, upload: local.is_none(), local, done: 0, total: 0, status: Status::Running });
        self.dirty.store(true, Ordering::Release);
        list.len() - 1
    }

    fn update(&self, index: usize, f: impl FnOnce(&mut Transfer)) {
        f(&mut self.transfers.lock().unwrap()[index]);
        self.dirty.store(true, Ordering::Release);
    }
}

pub struct FilesUi {
    _timer: slint::Timer,
}

pub fn install(window: &SessionWindow, files: Files, rt: Handle, host_name: &str) -> FilesUi {
    window.set_host_name(host_name.into());
    let listing: Rc<RefCell<Option<DirListing>>> = Rc::default();
    let progress = Arc::new(Progress::default());
    let model = Rc::new(VecModel::<TransferRow>::default());
    window.set_transfers(ModelRc::from(model.clone()));

    let load = {
        let weak = window.as_weak();
        let listing = listing.clone();
        let files = files.clone();
        let rt = rt.clone();
        Rc::new(move |path: String| {
            let Some(w) = weak.upgrade() else { return };
            w.set_files_loading(true);
            w.set_files_error(SharedString::new());
            let task = {
                let files = files.clone();
                rt.spawn(async move { files.list(&path).await })
            };
            let weak = weak.clone();
            let listing = listing.clone();
            let _ = slint::spawn_local(async move {
                let result = task.await;
                let Some(w) = weak.upgrade() else { return };
                w.set_files_loading(false);
                match result {
                    Ok(Ok(l)) => {
                        let m = w.global::<Messages>();
                        let rows: Vec<FileRow> = l
                            .entries
                            .iter()
                            .map(|e| FileRow {
                                name: e.name.as_str().into(),
                                subtitle: if e.is_dir { SharedString::new() } else { size_text(&m, e.size) },
                                is_dir: e.is_dir,
                            })
                            .collect();
                        w.set_files_path(l.path.as_str().into());
                        w.set_files_can_up(!l.parent.is_empty());
                        w.set_files(ModelRc::new(VecModel::from(rows)));
                        *listing.borrow_mut() = Some(l);
                    }
                    Ok(Err(e)) => w.set_files_error(e.to_string().into()),
                    Err(e) => w.set_files_error(e.to_string().into()),
                }
            });
        })
    };

    {
        let load = load.clone();
        let listing = listing.clone();
        window.on_files_opened(move || {
            if listing.borrow().is_none() {
                load(String::new());
            }
        });
    }
    {
        let load = load.clone();
        let listing = listing.clone();
        window.on_files_up(move || {
            let parent = listing.borrow().as_ref().map(|l| l.parent.clone());
            if let Some(parent) = parent.filter(|p| !p.is_empty()) {
                load(parent);
            }
        });
    }
    {
        let load = load.clone();
        let listing = listing.clone();
        let files = files.clone();
        let rt = rt.clone();
        let progress = progress.clone();
        window.on_files_activate(move |i| {
            let entry = {
                let listing = listing.borrow();
                let Some(l) = listing.as_ref() else { return };
                let Some(e) = l.entries.get(i as usize) else { return };
                (format!("{}{}{}", l.path.trim_end_matches(l.separator.as_str()), l.separator, e.name), e.clone())
            };
            let (path, entry) = entry;
            if entry.is_dir {
                load(path);
                return;
            }
            let target = free_local_path(&download_dir(), &entry.name);
            let index = progress.add(entry.name.clone(), Some(target.clone()));
            let files = files.clone();
            let progress = progress.clone();
            rt.spawn(async move {
                let reporter = progress.clone();
                let result = files
                    .download(&path, &target, |done, total| {
                        reporter.update(index, |t| (t.done, t.total) = (done, total))
                    })
                    .await;
                finish(&progress, index, result);
            });
        });
    }
    {
        let weak = window.as_weak();
        let files = files.clone();
        let rt = rt.clone();
        let progress = progress.clone();
        window.on_files_send(move || {
            let Some(w) = weak.upgrade() else { return };
            let title = w.global::<Messages>().invoke_choose_files().to_string();
            let files = files.clone();
            let progress = progress.clone();
            rt.spawn(async move {
                for path in pick_files(&title).await {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let index = progress.add(name, None);
                    let reporter = progress.clone();
                    let result = files
                        .upload(&path, |done, total| reporter.update(index, |t| (t.done, t.total) = (done, total)))
                        .await;
                    finish(&progress, index, result);
                }
            });
        });
    }

    {
        let progress = progress.clone();
        window.on_files_open_transfer(move |i| {
            let dir = progress.transfers.lock().unwrap().get(i as usize).and_then(|t| t.local.clone());
            if let Some(dir) = dir.as_deref().and_then(Path::parent) {
                crate::open_url(&dir.to_string_lossy());
            }
        });
    }

    let timer = slint::Timer::default();
    {
        let weak = window.as_weak();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            if !progress.dirty.swap(false, Ordering::AcqRel) {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            let m = w.global::<Messages>();
            let list = progress.transfers.lock().unwrap();
            let rows: Vec<TransferRow> = list
                .iter()
                .map(|t| TransferRow {
                    name: t.name.as_str().into(),
                    upload: t.upload,
                    progress: if t.total == 0 { 0.0 } else { t.done as f32 / t.total as f32 },
                    state: match t.status {
                        Status::Running => 0,
                        Status::Done => 1,
                        Status::Failed(_) => 2,
                    },
                    status: match &t.status {
                        Status::Running if t.total == 0 => m.invoke_transfer_waiting(),
                        Status::Running => m.invoke_transfer_progress(size_text(&m, t.done), size_text(&m, t.total)),
                        Status::Done if t.upload => m.invoke_uploaded(),
                        Status::Done => m.invoke_downloaded(),
                        Status::Failed(e) => m.invoke_transfer_failed(e.as_str().into()),
                    },
                })
                .collect();
            model.set_vec(rows);
        });
    }
    FilesUi { _timer: timer }
}

fn finish(progress: &Progress, index: usize, result: Result<(), FileError>) {
    progress.update(index, |t| {
        t.status = match result {
            Ok(()) => Status::Done,
            Err(e) => Status::Failed(e.to_string()),
        }
    });
}

/// Human-readable size with the unit translated by the UI.
fn size_text(m: &Messages<'_>, bytes: u64) -> SharedString {
    let (value, unit) = split_size(bytes);
    m.invoke_size(value.into(), unit)
}

fn split_size(bytes: u64) -> (String, i32) {
    let b = bytes as f64;
    let (x, unit) = match bytes {
        0..1000 => return (bytes.to_string(), 0),
        1000..1_000_000 => (b / 1e3, 1),
        1_000_000..1_000_000_000 => (b / 1e6, 2),
        _ => (b / 1e9, 3),
    };
    let text = if x < 10.0 { format!("{x:.1}") } else { format!("{x:.0}") };
    (if crate::uses_decimal_comma() { text.replace('.', ",") } else { text }, unit)
}

/// Downloads, or `$OKNO_DOWNLOAD_DIR` (tests).
fn download_dir() -> PathBuf {
    std::env::var_os("OKNO_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .or_else(dirs::download_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
}

/// `dir/name`, or `dir/stem (2).ext` … when taken.
fn free_local_path(dir: &Path, name: &str) -> PathBuf {
    let name = name.rsplit(['/', '\\']).next().unwrap_or("file");
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    (2..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).expect("some free name")
}

/// Asks the user for files to send.
async fn pick_files(title: &str) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use ashpd::desktop::file_chooser::SelectedFiles;
        let request = SelectedFiles::open_file().title(title).multiple(true).send().await;
        match request.and_then(|r| r.response()) {
            Ok(selected) => selected.uris().iter().filter_map(|u| file_uri_to_path(u.as_str())).collect(),
            Err(e) => {
                tracing::debug!("file chooser: {e}");
                Vec::new()
            }
        }
    }
    #[cfg(windows)]
    {
        rfd::AsyncFileDialog::new()
            .set_title(title)
            .pick_files()
            .await
            .map(|files| files.into_iter().map(|f| f.path().to_path_buf()).collect())
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = title;
        Vec::new()
    }
}

/// `file:///home/a%20b/x` → `/home/a b/x`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
    }
    #[cfg(not(unix))]
    {
        Some(PathBuf::from(String::from_utf8(out).ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uris() {
        assert_eq!(file_uri_to_path("file:///home/a%20b/%D0%AF.txt"), Some(PathBuf::from("/home/a b/Я.txt")));
        assert_eq!(file_uri_to_path("https://x"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(split_size(999), ("999".into(), 0));
        assert_eq!(split_size(1500).1, 1);
        assert_eq!(split_size(25_000_000), ("25".into(), 2));
    }
}
