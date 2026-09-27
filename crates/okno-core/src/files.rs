//! File transfer between client and host.
//!
//! Every request carries a client-chosen id echoed in all replies.
//!
//! ```text
//! list:      C ListDir(path)            → H Listing | Failed
//! download:  C Download{path}           → H Accept(size), Chunk…, Done(sha256) | Failed
//! upload:    C Upload{name, size}       → H Accept(0)
//!            C Chunk…, Done(sha256)     → H Done(sha256) | Failed
//! either:    C Cancel                   (stops the transfer, host drops partial files)
//! ```
//!
//! Uploads land in the host's incoming directory (Downloads) under a free
//! name; the data is written to `<name>.okno-part` and renamed only after
//! the checksum matched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use okno_net::Sender;
use okno_proto::envelope::Msg;
use okno_proto::file_msg::Op;
use okno_proto::{DirEntry, DirListing, FileChunk, FileMsg, FileOpen};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

pub const SERVICE_FILES: &str = "files";

/// Bytes per chunk message.
pub const CHUNK: usize = 256 * 1024;

const PART_SUFFIX: &str = ".okno-part";

fn msg(id: u64, op: Op) -> Msg {
    Msg::File(FileMsg { id, op: Some(op) })
}

// ---- Host ------------------------------------------------------------------

struct Upload {
    file: tokio::fs::File,
    part: PathBuf,
    target: PathBuf,
    size: u64,
    received: u64,
    hasher: Sha256,
}

/// Serves file requests of one session.
pub struct FileService {
    sender: Sender,
    incoming: PathBuf,
    uploads: HashMap<u64, Upload>,
    downloads: Arc<Mutex<HashMap<u64, AbortHandle>>>,
}

impl FileService {
    pub fn new(sender: Sender, incoming: PathBuf) -> Self {
        Self { sender, incoming, uploads: HashMap::new(), downloads: Arc::default() }
    }

    /// Where uploads go when nothing else is configured: Downloads, or
    /// `$OKNO_INCOMING_DIR` (tests).
    pub fn default_incoming() -> PathBuf {
        std::env::var_os("OKNO_INCOMING_DIR")
            .map(PathBuf::from)
            .or_else(dirs::download_dir)
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir)
    }

    pub async fn handle(&mut self, request: FileMsg) {
        let id = request.id;
        let Some(op) = request.op else { return };
        let result = match op {
            Op::ListDir(path) => self.list(id, path).await,
            Op::Download(open) => {
                self.download(id, open);
                Ok(())
            }
            Op::Upload(open) => self.start_upload(id, open).await,
            Op::Chunk(chunk) => self.write_chunk(id, chunk).await,
            Op::Done(sha) => self.finish_upload(id, sha).await,
            Op::Cancel(_) => {
                self.cancel(id).await;
                Ok(())
            }
            _ => Err("unexpected request".into()),
        };
        if let Err(e) = result {
            if let Some(upload) = self.uploads.remove(&id) {
                let _ = tokio::fs::remove_file(&upload.part).await;
            }
            let _ = self.sender.send(msg(id, Op::Failed(e))).await;
        }
    }

    async fn list(&self, id: u64, path: String) -> Result<(), String> {
        let listing = tokio::task::spawn_blocking(move || list_dir(&path)).await.map_err(|e| e.to_string())??;
        self.sender.send(msg(id, Op::Listing(listing))).await.map_err(|e| e.to_string())
    }

    fn download(&self, id: u64, open: FileOpen) {
        let sender = self.sender.clone();
        let downloads = self.downloads.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = send_file(&sender, id, Path::new(&open.path)).await {
                let _ = sender.send(msg(id, Op::Failed(e))).await;
            }
            downloads.lock().unwrap().remove(&id);
        });
        self.downloads.lock().unwrap().insert(id, task.abort_handle());
    }

    async fn start_upload(&mut self, id: u64, open: FileOpen) -> Result<(), String> {
        let name = safe_file_name(&open.path).ok_or("invalid file name")?;
        tokio::fs::create_dir_all(&self.incoming).await.map_err(|e| e.to_string())?;
        let target = free_path(&self.incoming, &name);
        let mut part = target.clone().into_os_string();
        part.push(PART_SUFFIX);
        let part = PathBuf::from(part);
        let file = tokio::fs::File::create(&part).await.map_err(|e| e.to_string())?;
        self.uploads.insert(id, Upload { file, part, target, size: open.size, received: 0, hasher: Sha256::new() });
        self.sender.send(msg(id, Op::Accept(0))).await.map_err(|e| e.to_string())
    }

    async fn write_chunk(&mut self, id: u64, chunk: FileChunk) -> Result<(), String> {
        let upload = self.uploads.get_mut(&id).ok_or("no such upload")?;
        if chunk.offset != upload.received || upload.received + chunk.data.len() as u64 > upload.size {
            return Err("chunk out of order".into());
        }
        upload.file.write_all(&chunk.data).await.map_err(|e| e.to_string())?;
        upload.hasher.update(&chunk.data);
        upload.received += chunk.data.len() as u64;
        Ok(())
    }

    async fn finish_upload(&mut self, id: u64, sha: Vec<u8>) -> Result<(), String> {
        let mut upload = self.uploads.remove(&id).ok_or("no such upload")?;
        upload.file.flush().await.map_err(|e| e.to_string())?;
        upload.file.sync_all().await.map_err(|e| e.to_string())?;
        drop(upload.file);
        let digest = upload.hasher.finalize().to_vec();
        if upload.received != upload.size || digest != sha {
            let _ = tokio::fs::remove_file(&upload.part).await;
            return Err("checksum mismatch".into());
        }
        // Another upload may have taken the name meanwhile.
        let target = if upload.target.exists() {
            free_path(upload.target.parent().unwrap_or(&self.incoming), &file_name(&upload.target))
        } else {
            upload.target
        };
        tokio::fs::rename(&upload.part, &target).await.map_err(|e| e.to_string())?;
        tracing::info!("received {}", target.display());
        self.sender.send(msg(id, Op::Done(digest))).await.map_err(|e| e.to_string())
    }

    async fn cancel(&mut self, id: u64) {
        if let Some(handle) = self.downloads.lock().unwrap().remove(&id) {
            handle.abort();
        }
        if let Some(upload) = self.uploads.remove(&id) {
            drop(upload.file);
            let _ = tokio::fs::remove_file(&upload.part).await;
        }
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        for (_, handle) in self.downloads.lock().unwrap().drain() {
            handle.abort();
        }
        for (_, upload) in self.uploads.drain() {
            let _ = std::fs::remove_file(&upload.part);
        }
    }
}

async fn send_file(sender: &Sender, id: u64, path: &Path) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    let size = file.metadata().await.map_err(|e| e.to_string())?.len();
    sender.send(msg(id, Op::Accept(size))).await.map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        // `send` waits while the bulk queue is full: natural backpressure.
        sender
            .send(msg(id, Op::Chunk(FileChunk { offset, data: buf[..n].to_vec() })))
            .await
            .map_err(|e| e.to_string())?;
        offset += n as u64;
    }
    sender.send(msg(id, Op::Done(hasher.finalize().to_vec()))).await.map_err(|e| e.to_string())
}

fn list_dir(path: &str) -> Result<DirListing, String> {
    let dir = if path.is_empty() { dirs::home_dir().ok_or("no home directory")? } else { PathBuf::from(path) };
    let dir = dir.canonicalize().map_err(|e| e.to_string())?;
    let mut entries: Vec<DirEntry> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Hidden files, as file managers do by default.
            if name.starts_with('.') {
                return None;
            }
            let meta = entry.metadata().ok()?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(DirEntry { name, is_dir: meta.is_dir(), size: if meta.is_dir() { 0 } else { meta.len() }, modified })
        })
        .collect();
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(DirListing {
        path: dir.to_string_lossy().into_owned(),
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        separator: std::path::MAIN_SEPARATOR.to_string(),
        entries,
    })
}

/// The last path component, if it is a plain file name.
fn safe_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next()?.trim();
    let bad = base.is_empty() || base == "." || base == ".." || base.chars().any(|c| c.is_control() || c == ':');
    (!bad).then(|| base.chars().take(200).collect())
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// `dir/name`, or `dir/stem (2).ext` … when taken.
fn free_path(dir: &Path, name: &str) -> PathBuf {
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

// ---- Client ----------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{0}")]
    Remote(String),
    #[error("{0}")]
    Local(#[from] std::io::Error),
    #[error("connection closed")]
    Closed,
    #[error("the file changed in transit")]
    Checksum,
    #[error("unexpected reply")]
    Protocol,
}

type Routes = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Op>>>>;

/// Client side of file transfer; cheap to clone.
#[derive(Clone)]
pub struct Files {
    sender: Sender,
    routes: Routes,
    next: Arc<AtomicU64>,
}

/// Removes a route when a request ends.
struct Route {
    id: u64,
    routes: Routes,
    replies: mpsc::UnboundedReceiver<Op>,
}

impl Drop for Route {
    fn drop(&mut self) {
        self.routes.lock().unwrap().remove(&self.id);
    }
}

impl Route {
    async fn next(&mut self) -> Result<Op, FileError> {
        match self.replies.recv().await {
            Some(Op::Failed(e)) => Err(FileError::Remote(e)),
            Some(op) => Ok(op),
            None => Err(FileError::Closed),
        }
    }
}

impl Files {
    pub(crate) fn new(sender: Sender) -> Self {
        Self { sender, routes: Arc::default(), next: Arc::new(AtomicU64::new(1)) }
    }

    /// Delivers a reply from the host; called by the session reader.
    pub(crate) fn dispatch(&self, reply: FileMsg) {
        if let (Some(route), Some(op)) = (self.routes.lock().unwrap().get(&reply.id), reply.op) {
            let _ = route.send(op);
        }
    }

    fn open(&self) -> Route {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, replies) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().insert(id, tx);
        Route { id, routes: self.routes.clone(), replies }
    }

    async fn send(&self, id: u64, op: Op) -> Result<(), FileError> {
        self.sender.send(msg(id, op)).await.map_err(|_| FileError::Closed)
    }

    /// Lists a host directory; an empty path is the host user's home.
    pub async fn list(&self, path: &str) -> Result<DirListing, FileError> {
        let mut route = self.open();
        self.send(route.id, Op::ListDir(path.to_owned())).await?;
        match route.next().await? {
            Op::Listing(listing) => Ok(listing),
            _ => Err(FileError::Protocol),
        }
    }

    /// Copies a host file to `local`. `progress(done, total)` is called per
    /// chunk. The file appears under its final name only when complete.
    pub async fn download(&self, remote: &str, local: &Path, progress: impl Fn(u64, u64)) -> Result<(), FileError> {
        let mut route = self.open();
        self.send(route.id, Op::Download(FileOpen { path: remote.to_owned(), ..Default::default() })).await?;
        let total = match route.next().await? {
            Op::Accept(size) => size,
            _ => return Err(FileError::Protocol),
        };
        let mut part = local.as_os_str().to_owned();
        part.push(PART_SUFFIX);
        let part = PathBuf::from(part);
        let result = async {
            let mut file = tokio::fs::File::create(&part).await?;
            let mut hasher = Sha256::new();
            let mut done = 0u64;
            progress(0, total);
            loop {
                match route.next().await? {
                    Op::Chunk(chunk) => {
                        if chunk.offset != done {
                            return Err(FileError::Protocol);
                        }
                        file.write_all(&chunk.data).await?;
                        hasher.update(&chunk.data);
                        done += chunk.data.len() as u64;
                        progress(done, total);
                    }
                    Op::Done(sha) => {
                        file.sync_all().await?;
                        if done != total || hasher.finalize().as_slice() != sha.as_slice() {
                            return Err(FileError::Checksum);
                        }
                        return Ok(());
                    }
                    _ => return Err(FileError::Protocol),
                }
            }
        }
        .await;
        match result {
            Ok(()) => {
                tokio::fs::rename(&part, local).await?;
                Ok(())
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                let _ = self.send(route.id, Op::Cancel(true)).await;
                Err(e)
            }
        }
    }

    /// Sends a local file to the host's incoming directory.
    pub async fn upload(&self, local: &Path, progress: impl Fn(u64, u64)) -> Result<(), FileError> {
        let mut file = tokio::fs::File::open(local).await?;
        let total = file.metadata().await?.len();
        let mut route = self.open();
        let open = FileOpen { path: file_name(local), size: total, ..Default::default() };
        self.send(route.id, Op::Upload(open)).await?;
        match route.next().await? {
            Op::Accept(_) => {}
            _ => return Err(FileError::Protocol),
        }
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        let mut buf = vec![0u8; CHUNK];
        progress(0, total);
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            self.send(route.id, Op::Chunk(FileChunk { offset, data: buf[..n].to_vec() })).await?;
            offset += n as u64;
            progress(offset, total);
            // A failure reply (disk full) arrives while we keep sending.
            if let Ok(Op::Failed(e)) = route.replies.try_recv() {
                return Err(FileError::Remote(e));
            }
        }
        let digest = hasher.finalize().to_vec();
        self.send(route.id, Op::Done(digest.clone())).await?;
        match route.next().await? {
            Op::Done(sha) if sha == digest => Ok(()),
            Op::Done(_) => Err(FileError::Checksum),
            _ => Err(FileError::Protocol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_sanitised() {
        assert_eq!(safe_file_name("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe_file_name("/etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(safe_file_name("..\\..\\evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(safe_file_name(".."), None);
        assert_eq!(safe_file_name(""), None);
        assert_eq!(safe_file_name("C:"), None);
    }

    #[test]
    fn free_path_adds_counter() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(free_path(dir.path(), "a.txt"), dir.path().join("a.txt"));
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        assert_eq!(free_path(dir.path(), "a.txt"), dir.path().join("a (2).txt"));
        std::fs::write(dir.path().join("a (2).txt"), b"x").unwrap();
        assert_eq!(free_path(dir.path(), "a.txt"), dir.path().join("a (3).txt"));
        std::fs::write(dir.path().join("noext"), b"x").unwrap();
        assert_eq!(free_path(dir.path(), "noext"), dir.path().join("noext (2)"));
    }

    #[test]
    fn lists_directories_first_without_hidden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        std::fs::create_dir(dir.path().join("Zeta")).unwrap();
        let listing = list_dir(dir.path().to_str().unwrap()).unwrap();
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Zeta", "b.txt"]);
        assert_eq!(listing.entries[1].size, 5);
        assert!(!listing.parent.is_empty());
    }
}
