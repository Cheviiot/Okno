//! Remote terminal: a PTY on the host running the user's shell.
//!
//! ```text
//! C Open{cols, rows}   → H Data…            (shell output)
//! C Data               → written to the PTY (keystrokes)
//! C Resize{cols, rows}
//! C Exit               → the shell is killed
//! H Exit(code)         when the shell ends
//! ```

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use okno_net::Sender;
use okno_proto::envelope::Msg;
use okno_proto::terminal_msg::Op;
use okno_proto::{TerminalMsg, TerminalSize};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

pub const SERVICE_TERMINAL: &str = "terminal";

fn msg(id: u32, op: Op) -> Msg {
    Msg::Terminal(TerminalMsg { id, op: Some(op) })
}

fn pty_size(size: &TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows.clamp(2, 500) as u16,
        cols: size.cols.clamp(2, 1000) as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Keystroke messages waiting for a shell that does not read its input.
const INPUT_QUEUE: usize = 256;

struct Term {
    master: Box<dyn MasterPty + Send>,
    /// To the thread writing into the PTY; a write blocks while the shell
    /// is not reading, which must not stall the session.
    input: std::sync::mpsc::SyncSender<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

/// Terminals of one host session.
pub struct TerminalService {
    sender: Sender,
    terms: HashMap<u32, Term>,
}

impl TerminalService {
    pub fn new(sender: Sender) -> Self {
        Self { sender, terms: HashMap::new() }
    }

    pub async fn handle(&mut self, request: TerminalMsg) {
        let id = request.id;
        match request.op {
            Some(Op::Open(size)) => {
                if self.terms.contains_key(&id) {
                    tracing::debug!("terminal {id}: id already in use");
                    return;
                }
                if let Err(e) = self.open(id, &size) {
                    tracing::warn!("terminal failed to start: {e}");
                    let _ = self.sender.send(msg(id, Op::Data(format!("okno: {e}\r\n").into_bytes()))).await;
                    let _ = self.sender.send(msg(id, Op::Exit(-1))).await;
                }
            }
            Some(Op::Data(bytes)) => {
                if let Some(term) = self.terms.get(&id)
                    && term.input.try_send(bytes).is_err()
                {
                    tracing::warn!("terminal {id}: shell is not reading, input dropped");
                }
            }
            Some(Op::Resize(size)) => {
                if let Some(term) = self.terms.get(&id) {
                    let _ = term.master.resize(pty_size(&size));
                }
            }
            Some(Op::Exit(_)) => {
                if let Some(mut term) = self.terms.remove(&id) {
                    let _ = term.killer.kill();
                }
            }
            None => {}
        }
    }

    fn open(&mut self, id: u32, size: &TerminalSize) -> anyhow::Result<()> {
        let pair = native_pty_system().openpty(pty_size(size))?;
        let mut cmd =
            if cfg!(windows) { CommandBuilder::new("powershell.exe") } else { CommandBuilder::new_default_prog() };
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let killer = child.clone_killer();

        let (input, keys) = std::sync::mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
        std::thread::Builder::new().name("okno-pty-in".into()).spawn(move || {
            for bytes in keys {
                if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })?;

        // PTY reads block, so a thread pumps output into the async sender;
        // the same thread reports the exit code once output ends.
        let sender = self.sender.clone();
        let rt = Handle::current();
        std::thread::Builder::new().name("okno-pty".into()).spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if rt.block_on(sender.send(msg(id, Op::Data(buf[..n].to_vec())))).is_err() {
                            break;
                        }
                    }
                }
            }
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let _ = rt.block_on(sender.send(msg(id, Op::Exit(code))));
        })?;
        self.terms.insert(id, Term { master: pair.master, input, killer });
        Ok(())
    }
}

impl Drop for TerminalService {
    fn drop(&mut self) {
        for (_, mut term) in self.terms.drain() {
            let _ = term.killer.kill();
        }
    }
}

// ---- Client ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    Exited(i32),
}

type Routes = Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<TerminalEvent>>>>;

/// Client side of remote terminals; cheap to clone.
#[derive(Clone)]
pub struct Terminals {
    sender: Sender,
    routes: Routes,
    next: Arc<AtomicU32>,
}

impl Terminals {
    pub(crate) fn new(sender: Sender) -> Self {
        Self { sender, routes: Arc::default(), next: Arc::new(AtomicU32::new(1)) }
    }

    pub(crate) fn dispatch(&self, reply: TerminalMsg) {
        let event = match reply.op {
            Some(Op::Data(bytes)) => TerminalEvent::Output(bytes),
            Some(Op::Exit(code)) => TerminalEvent::Exited(code),
            _ => return,
        };
        let mut routes = self.routes.lock().unwrap();
        if let Some(route) = routes.get(&reply.id) {
            let ended = matches!(event, TerminalEvent::Exited(_));
            let _ = route.send(event);
            if ended {
                routes.remove(&reply.id);
            }
        }
    }

    /// Starts a shell on the host.
    pub fn open(&self, cols: u32, rows: u32) -> (RemoteTerminal, mpsc::UnboundedReceiver<TerminalEvent>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, events) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().insert(id, tx);
        let terminal = RemoteTerminal { id, sender: self.sender.clone(), routes: self.routes.clone() };
        terminal.send(Op::Open(TerminalSize { cols, rows }));
        (terminal, events)
    }
}

/// Handle to one remote shell; dropping it kills the shell.
pub struct RemoteTerminal {
    id: u32,
    sender: Sender,
    routes: Routes,
}

impl RemoteTerminal {
    fn send(&self, op: Op) {
        // Terminal traffic is small; drop input rather than block the UI
        // when the bulk queue is momentarily full.
        let _ = self.sender.try_send(msg(self.id, op));
    }

    pub fn input(&self, bytes: Vec<u8>) {
        self.send(Op::Data(bytes));
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        self.send(Op::Resize(TerminalSize { cols, rows }));
    }
}

impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        self.routes.lock().unwrap().remove(&self.id);
        self.send(Op::Exit(0));
    }
}
