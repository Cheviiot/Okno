//! System file and folder choosers (XDG portal on Linux, native on Windows).

use std::path::PathBuf;

/// Asks the user for files to send.
pub async fn pick_files(title: &str) -> Vec<PathBuf> {
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

/// Asks the user for a folder.
pub async fn pick_folder(title: &str) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use ashpd::desktop::file_chooser::SelectedFiles;
        let request = SelectedFiles::open_file().title(title).directory(true).send().await;
        match request.and_then(|r| r.response()) {
            Ok(selected) => selected.uris().first().and_then(|u| file_uri_to_path(u.as_str())),
            Err(e) => {
                tracing::debug!("folder chooser: {e}");
                None
            }
        }
    }
    #[cfg(windows)]
    {
        rfd::AsyncFileDialog::new().set_title(title).pick_folder().await.map(|f| f.path().to_path_buf())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = title;
        None
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
}
