//! The local clipboard, owned by one thread.
//!
//! On X11 (and XWayland) the clipboard content lives in the process that
//! set it, so the `arboard::Clipboard` must stay alive; a dedicated thread
//! keeps it and serves requests from the UI and network threads.

use std::sync::mpsc;
use std::time::Duration;

enum Command {
    Set(String),
    Get(mpsc::Sender<Option<String>>),
}

#[derive(Clone)]
pub struct LocalClipboard {
    commands: mpsc::Sender<Command>,
}

impl LocalClipboard {
    pub fn new() -> Self {
        let (commands, requests) = mpsc::channel::<Command>();
        let _ = std::thread::Builder::new().name("okno-local-clipboard".into()).spawn(move || {
            let mut clipboard = match arboard::Clipboard::new() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("local clipboard unavailable: {e}");
                    None
                }
            };
            while let Ok(command) = requests.recv() {
                match (command, clipboard.as_mut()) {
                    (Command::Set(text), Some(c)) => {
                        if let Err(e) = c.set_text(text) {
                            tracing::debug!("clipboard set failed: {e}");
                        }
                    }
                    (Command::Get(reply), Some(c)) => {
                        let _ = reply.send(c.get_text().ok());
                    }
                    (Command::Get(reply), None) => {
                        let _ = reply.send(None);
                    }
                    (Command::Set(_), None) => {}
                }
            }
        });
        Self { commands }
    }

    pub fn set(&self, text: String) {
        let _ = self.commands.send(Command::Set(text));
    }

    /// Current text, or `None` when empty, not text, or too slow.
    pub fn get(&self) -> Option<String> {
        let (reply, answer) = mpsc::channel();
        self.commands.send(Command::Get(reply)).ok()?;
        answer.recv_timeout(Duration::from_millis(300)).ok().flatten()
    }
}
