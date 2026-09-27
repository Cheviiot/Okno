//! File transfer between client and host.
//!
//! Every request carries a client-chosen id echoed in all replies.
//!
//! ```text
//! list:      C ListDir(path)            → H Listing | Failed
//! download:  C Download{path}           → H Accept(size), Chunk…, Done(sha256) | Failed
//!            C Ack(bytes written)       (the host stays at most WINDOW ahead)
//! upload:    C Upload{name, size}       → H Accept(0)
//!            C Chunk…, Done(sha256)     → H Done(sha256) | Failed
//! either:    C Cancel                   (stops the transfer, host drops partial files)
//! ```
//!
//! Uploads land in the host's incoming directory (Downloads) under a free
//! name; the data is written to `<name>.okno-part` and renamed only after
//! the checksum matched. Request ids already in use are refused.

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
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

pub const SERVICE_FILES: &str = "files";

/// Bytes per chunk message.
pub const CHUNK: usize = 256 * 1024;

/// How far a download may run ahead of the receiver's acknowledgements.
const WINDOW: u64 = 32 * CHUNK as u64;

/// Replies buffered per client request; more than a window's worth of
/// chunks means the host ignores flow control.
const ROUTE_DEPTH: usize = 64;

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

struct Download {
    task: AbortHandle,
    acked: watch::Sender<u64>,
}

/// Serves file requests of one session.
pub struct FileService {
    sender: Sender,
    incoming: PathBuf,
    uploads: HashMap<u64, Upload>,
    downloads: Arc<Mutex<HashMap<u64, Download>>>,
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
            Op::Download(open) => self.download(id, open),
            Op::Upload(open) => self.start_upload(id, open).await,
            Op::Chunk(chunk) => self.write_chunk(id, chunk).await,
            Op::Done(sha) => self.finish_upload(id, sha).await,
            Op::Cancel(_) => {
                self.cancel(id).await;
                Ok(())
            }
            Op::Ack(written) => {
                if let Some(download) = self.downloads.lock().unwrap().get(&id) {
                    download.acked.send_if_modified(|acked| {
                        let newer = written > *acked;
                        *acked = (*acked).max(written);
                        newer
                    });
                }
                Ok(())
            }
            _ => Err("unexpected request".into()),
        };
        if let Err(e) = result {
            tracing::debug!("file request {id}: {e}");
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

    fn download(&self, id: u64, open: FileOpen) -> Result<(), String> {
        // Hold the lock across spawn so a quick task cannot finish (and
        // remove itself) before it is registered.
        let mut downloads = self.downloads.lock().unwrap();
        if downloads.contains_key(&id) {
            return Err("request id in use".into());
        }
        let sender = self.sender.clone();
        let registry = self.downloads.clone();
        let (acked, acks) = watch::channel(0);
        let task = tokio::spawn(async move {
            if let Err(e) = send_file(&sender, id, Path::new(&open.path), acks).await {
                let _ = sender.send(msg(id, Op::Failed(e))).await;
            }
            registry.lock().unwrap().remove(&id);
        });
        downloads.insert(id, Download { task: task.abort_handle(), acked });
        Ok(())
    }

    async fn start_upload(&mut self, id: u64, open: FileOpen) -> Result<(), String> {
        if self.uploads.contains_key(&id) {
            return Err("request id in use".into());
        }
        let name = safe_file_name(&open.path).ok_or("invalid file name")?;
        tokio::fs::create_dir_all(&self.incoming).await.map_err(|e| e.to_string())?;
        let incoming = self.incoming.clone();
        let (file, target, part) =
            tokio::task::spawn_blocking(move || claim(&incoming, &name)).await.map_err(|e| e.to_string())??;
        let file = tokio::fs::File::from_std(file);
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
        let (part, target) = (upload.part, upload.target);
        let incoming = self.incoming.clone();
        let target = tokio::task::spawn_blocking(move || publish(&part, &target, &incoming))
            .await
            .map_err(|e| e.to_string())??;
        tracing::info!("received {}", target.display());
        self.sender.send(msg(id, Op::Done(digest))).await.map_err(|e| e.to_string())
    }

    async fn cancel(&mut self, id: u64) {
        if let Some(download) = self.downloads.lock().unwrap().remove(&id) {
            download.task.abort();
        }
        if let Some(upload) = self.uploads.remove(&id) {
            drop(upload.file);
            let _ = tokio::fs::remove_file(&upload.part).await;
        }
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        for (_, download) in self.downloads.lock().unwrap().drain() {
            download.task.abort();
        }
        for (_, upload) in self.uploads.drain() {
            let _ = std::fs::remove_file(&upload.part);
        }
    }
}

async fn send_file(sender: &Sender, id: u64, path: &Path, mut acked: watch::Receiver<u64>) -> Result<(), String> {
    let file = tokio::fs::File::open(path).await.map_err(|e| e.to_string())?;
    let meta = file.metadata().await.map_err(|e| e.to_string())?;
    // Not a pipe or a device that never ends.
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let size = meta.len();
    // A file that grows meanwhile is sent as it was when opened.
    let mut file = file.take(size);
    sender.send(msg(id, Op::Accept(size))).await.map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        acked.wait_for(|&acked| offset.saturating_sub(acked) < WINDOW).await.map_err(|_| "cancelled")?;
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

/// A name the other side sent, made safe to create in a directory on this
/// system: only the last path component, no drive prefixes, streams or
/// characters Windows rejects, no reserved device names (`CON`, `NUL` …).
/// `None` when nothing usable is left.
pub fn safe_file_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next()?;
    let mut clean: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .take(200)
        .collect();
    // Windows drops trailing dots and spaces, which would change the name.
    clean.truncate(clean.trim_end_matches(['.', ' ']).len());
    let clean = clean.trim_start().to_owned();
    if clean.is_empty() || clean == "." || clean == ".." {
        return None;
    }
    let stem = clean.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem[3..].chars().all(|c| c.is_ascii_digit() || matches!(c, '¹' | '²' | '³')));
    Some(if reserved { format!("_{clean}") } else { clean })
}

/// `dir/name`, then `dir/stem (2).ext`, `dir/stem (3).ext` …
fn candidates(dir: &Path, name: &str) -> impl Iterator<Item = PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_owned(), format!(".{e}")),
        _ => (name.to_owned(), String::new()),
    };
    let dir = dir.to_owned();
    std::iter::once(dir.join(name)).chain((2u32..).map(move |n| dir.join(format!("{stem} ({n}){ext}"))))
}

/// The first of [`candidates`] that does not exist. Someone may still take
/// it before it is used; [`claim`] does not have that race.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    candidates(dir, name).find(|p| !p.exists()).expect("some free name")
}

fn part_path(target: &Path) -> PathBuf {
    let mut part = target.as_os_str().to_owned();
    part.push(PART_SUFFIX);
    PathBuf::from(part)
}

/// Creates the part file for a free name atomically, so two uploads of the
/// same name never share one. Returns the file, the target and the part.
fn claim(dir: &Path, name: &str) -> Result<(std::fs::File, PathBuf, PathBuf), String> {
    for target in candidates(dir, name).take(10_000) {
        if target.exists() {
            continue;
        }
        let part = part_path(&target);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&part) {
            Ok(file) => return Ok((file, target, part)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("no free file name".into())
}

/// Moves a finished part file to `target`, or to the next free name if
/// something appeared there meanwhile; never replaces an existing file.
fn publish(part: &Path, target: &Path, dir: &Path) -> Result<PathBuf, String> {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for candidate in std::iter::once(target.to_owned()).chain(candidates(dir, &name).skip(1)).take(10_000) {
        // A hard link fails if the name exists, unlike rename.
        match std::fs::hard_link(part, &candidate) {
            Ok(()) => {
                let _ = std::fs::remove_file(part);
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // File systems without hard links (FAT): check, then rename.
            Err(_) if !candidate.exists() => {
                return std::fs::rename(part, &candidate).map(|()| candidate).map_err(|e| e.to_string());
            }
            Err(_) => continue,
        }
    }
    Err("no free file name".into())
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
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

type Routes = Arc<Mutex<HashMap<u64, mpsc::Sender<Op>>>>;

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
    replies: mpsc::Receiver<Op>,
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
            // Full only when the host ignores flow control; the transfer
            // then notices the gap and fails.
            if route.try_send(op).is_err() {
                tracing::debug!("file request {}: reply dropped", reply.id);
            }
        }
    }

    fn open(&self) -> Route {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, replies) = mpsc::channel(ROUTE_DEPTH);
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
        let part = part_path(local);
        let result = async {
            let mut file = tokio::fs::File::create(&part).await?;
            let mut hasher = Sha256::new();
            let mut done = 0u64;
            progress(0, total);
            loop {
                match route.next().await? {
                    Op::Chunk(chunk) => {
                        // More than announced would fill the disk.
                        if chunk.offset != done || done + chunk.data.len() as u64 > total {
                            return Err(FileError::Protocol);
                        }
                        file.write_all(&chunk.data).await?;
                        hasher.update(&chunk.data);
                        done += chunk.data.len() as u64;
                        progress(done, total);
                        self.send(route.id, Op::Ack(done)).await?;
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
        assert_eq!(safe_file_name("C:version.dll").as_deref(), Some("C_version.dll"));
        assert_eq!(safe_file_name("a.txt:stream").as_deref(), Some("a.txt_stream"));
        assert_eq!(safe_file_name("con").as_deref(), Some("_con"));
        assert_eq!(safe_file_name("NUL.txt").as_deref(), Some("_NUL.txt"));
        assert_eq!(safe_file_name("com1.log").as_deref(), Some("_com1.log"));
        assert_eq!(safe_file_name("console.log").as_deref(), Some("console.log"));
        assert_eq!(safe_file_name("notes. . ").as_deref(), Some("notes"));
        assert_eq!(safe_file_name("..."), None);
        assert_eq!(safe_file_name("12:30 plan.txt").as_deref(), Some("12_30 plan.txt"));
    }

    #[test]
    fn claims_and_publishes_without_replacing() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, target_a, part_a) = claim(dir.path(), "x.bin").unwrap();
        let (_b, target_b, part_b) = claim(dir.path(), "x.bin").unwrap();
        assert_eq!(target_a, dir.path().join("x.bin"));
        assert_eq!(target_b, dir.path().join("x (2).bin"));
        std::fs::write(&part_a, b"a").unwrap();
        std::fs::write(&part_b, b"b").unwrap();
        // Something else took both names meanwhile.
        std::fs::write(&target_a, b"other").unwrap();
        std::fs::write(&target_b, b"other").unwrap();
        let published = publish(&part_a, &target_a, dir.path()).unwrap();
        assert_eq!(published, dir.path().join("x (3).bin"));
        assert_eq!(std::fs::read(&published).unwrap(), b"a");
        assert_eq!(std::fs::read(&target_a).unwrap(), b"other");
        assert!(!part_a.exists());
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
