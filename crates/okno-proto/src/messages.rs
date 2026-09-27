//! Message definitions. Keep in sync with `docs/protocol.md`.

/// Top-level message. Every application frame carries exactly one.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Envelope {
    #[prost(oneof = "envelope::Msg", tags = "1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21")]
    pub msg: ::core::option::Option<envelope::Msg>,
}

pub mod envelope {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Msg {
        // Session setup.
        #[prost(message, tag = "1")]
        Hello(super::Hello),
        #[prost(message, tag = "2")]
        Login(super::Login),
        #[prost(message, tag = "3")]
        LoginResult(super::LoginResult),
        #[prost(message, tag = "4")]
        HostInfo(super::HostInfo),
        #[prost(message, tag = "5")]
        Ping(super::Ping),
        #[prost(message, tag = "6")]
        Pong(super::Ping),
        #[prost(message, tag = "7")]
        Close(super::Close),
        #[prost(message, tag = "8")]
        Error(super::ErrorMsg),

        // Remote desktop.
        #[prost(message, tag = "10")]
        VideoStart(super::VideoStart),
        #[prost(message, tag = "11")]
        VideoStop(super::VideoStop),
        #[prost(message, tag = "12")]
        KeyframeRequest(super::KeyframeRequest),
        #[prost(message, tag = "13")]
        Video(super::VideoFrame),
        #[prost(message, tag = "14")]
        Input(super::InputEvent),
        #[prost(message, tag = "15")]
        Clipboard(super::ClipboardText),

        // Audio.
        #[prost(message, tag = "16")]
        AudioControl(super::AudioControl),
        #[prost(message, tag = "17")]
        Audio(super::AudioPacket),

        // Services.
        #[prost(message, tag = "18")]
        File(super::FileMsg),
        #[prost(message, tag = "19")]
        Terminal(super::TerminalMsg),
        #[prost(message, tag = "20")]
        Tunnel(super::TunnelMsg),
        #[prost(message, tag = "21")]
        Stats(super::Stats),
    }
}

// ---------------------------------------------------------------------------
// Session setup

/// First message of both sides after the Noise handshake.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(string, tag = "2")]
    pub device_name: ::prost::alloc::string::String,
    /// `linux` or `windows`.
    #[prost(string, tag = "3")]
    pub os: ::prost::alloc::string::String,
    #[prost(string, repeated, tag = "4")]
    pub capabilities: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}

/// Client credentials; only ever sent inside the encrypted channel.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Login {
    #[prost(string, tag = "1")]
    pub username: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub password: ::prost::alloc::string::String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum LoginStatus {
    Unspecified = 0,
    Ok = 1,
    BadCredentials = 2,
    /// Too many failures; retry after `retry_after_ms`.
    Throttled = 3,
    /// Too many logins in progress at once.
    Busy = 4,
    /// The host has no password configured.
    NotConfigured = 5,
    /// The person at the host declined (or did not answer) the request.
    Denied = 6,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct LoginResult {
    #[prost(enumeration = "LoginStatus", tag = "1")]
    pub status: i32,
    #[prost(uint64, tag = "2")]
    pub retry_after_ms: u64,
}

/// Sent by the host after a successful login.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct HostInfo {
    #[prost(message, repeated, tag = "1")]
    pub displays: ::prost::alloc::vec::Vec<Display>,
    /// MAC addresses for Wake-on-LAN, as `aa:bb:cc:dd:ee:ff`.
    #[prost(string, repeated, tag = "2")]
    pub mac_addresses: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
    #[prost(string, repeated, tag = "3")]
    pub services: ::prost::alloc::vec::Vec<::prost::alloc::string::String>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Display {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub name: ::prost::alloc::string::String,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(bool, tag = "5")]
    pub primary: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Ping {
    #[prost(uint64, tag = "1")]
    pub nonce: u64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Close {
    #[prost(string, tag = "1")]
    pub reason: ::prost::alloc::string::String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ErrorMsg {
    #[prost(string, tag = "1")]
    pub message: ::prost::alloc::string::String,
}

// ---------------------------------------------------------------------------
// Video and input

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum Codec {
    Unspecified = 0,
    H264 = 1,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct VideoStart {
    #[prost(uint32, tag = "1")]
    pub display: u32,
    #[prost(uint32, tag = "2")]
    pub max_fps: u32,
    /// Target bitrate in kbit/s; 0 lets the host choose.
    #[prost(uint32, tag = "3")]
    pub bitrate_kbps: u32,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct VideoStop {}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct KeyframeRequest {}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct VideoFrame {
    #[prost(uint32, tag = "1")]
    pub display: u32,
    #[prost(enumeration = "Codec", tag = "2")]
    pub codec: i32,
    #[prost(uint32, tag = "3")]
    pub width: u32,
    #[prost(uint32, tag = "4")]
    pub height: u32,
    #[prost(bool, tag = "5")]
    pub keyframe: bool,
    /// Capture time on the host clock, microseconds.
    #[prost(uint64, tag = "6")]
    pub pts_us: u64,
    #[prost(bytes = "vec", tag = "7")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct InputEvent {
    #[prost(oneof = "input_event::Event", tags = "1, 2, 3, 4")]
    pub event: ::core::option::Option<input_event::Event>,
}

pub mod input_event {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        Motion(super::PointerMotion),
        #[prost(message, tag = "2")]
        Button(super::PointerButton),
        #[prost(message, tag = "3")]
        Scroll(super::PointerScroll),
        #[prost(message, tag = "4")]
        Key(super::KeyEvent),
    }
}

/// Absolute pointer position, normalised to 0.0..=1.0 of the display.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PointerMotion {
    #[prost(uint32, tag = "1")]
    pub display: u32,
    #[prost(double, tag = "2")]
    pub x: f64,
    #[prost(double, tag = "3")]
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, ::prost::Enumeration)]
#[repr(i32)]
pub enum MouseButton {
    Unspecified = 0,
    Left = 1,
    Right = 2,
    Middle = 3,
    Back = 4,
    Forward = 5,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PointerButton {
    #[prost(enumeration = "MouseButton", tag = "1")]
    pub button: i32,
    #[prost(bool, tag = "2")]
    pub pressed: bool,
}

/// Scroll in logical units: `steps_*` are wheel clicks, `dx`/`dy` smooth
/// deltas in pixels. Either may be zero.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct PointerScroll {
    #[prost(double, tag = "1")]
    pub dx: f64,
    #[prost(double, tag = "2")]
    pub dy: f64,
    #[prost(sint32, tag = "3")]
    pub steps_x: i32,
    #[prost(sint32, tag = "4")]
    pub steps_y: i32,
}

/// Physical key, identified by its Linux evdev code (`KEY_*`), which both
/// platforms translate to and from.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct KeyEvent {
    #[prost(uint32, tag = "1")]
    pub evdev_code: u32,
    #[prost(bool, tag = "2")]
    pub pressed: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ClipboardText {
    #[prost(string, tag = "1")]
    pub text: ::prost::alloc::string::String,
}

// ---------------------------------------------------------------------------
// Audio

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AudioControl {
    #[prost(bool, tag = "1")]
    pub enabled: bool,
    #[prost(uint32, tag = "2")]
    pub sample_rate: u32,
    #[prost(uint32, tag = "3")]
    pub channels: u32,
}

/// One Opus packet.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct AudioPacket {
    #[prost(uint64, tag = "1")]
    pub seq: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}

// ---------------------------------------------------------------------------
// Services

/// File transfer. Requests carry a client-chosen `id` echoed in replies.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileMsg {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(oneof = "file_msg::Op", tags = "2, 3, 4, 5, 6, 7, 8, 9, 10")]
    pub op: ::core::option::Option<file_msg::Op>,
}

pub mod file_msg {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Op {
        /// Ask for the entries of a directory; empty path means roots.
        #[prost(string, tag = "2")]
        ListDir(::prost::alloc::string::String),
        #[prost(message, tag = "3")]
        Listing(super::DirListing),
        /// Start sending a file from its owner.
        #[prost(message, tag = "4")]
        Download(super::FileOpen),
        /// Announce a file about to be written on the receiver.
        #[prost(message, tag = "5")]
        Upload(super::FileOpen),
        #[prost(message, tag = "6")]
        Chunk(super::FileChunk),
        /// Transfer finished; carries the SHA-256 of the whole file.
        #[prost(bytes = "vec", tag = "7")]
        Done(::prost::alloc::vec::Vec<u8>),
        #[prost(string, tag = "8")]
        Failed(::prost::alloc::string::String),
        #[prost(bool, tag = "9")]
        Cancel(bool),
        /// Receiver acknowledges `Upload` and states where to resume.
        #[prost(uint64, tag = "10")]
        Accept(u64),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DirListing {
    /// Absolute path of the listed directory on the host.
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
    #[prost(message, repeated, tag = "2")]
    pub entries: ::prost::alloc::vec::Vec<DirEntry>,
    /// Parent directory; empty at the root.
    #[prost(string, tag = "3")]
    pub parent: ::prost::alloc::string::String,
    /// Path separator of the host, to build child paths.
    #[prost(string, tag = "4")]
    pub separator: ::prost::alloc::string::String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct DirEntry {
    #[prost(string, tag = "1")]
    pub name: ::prost::alloc::string::String,
    #[prost(bool, tag = "2")]
    pub is_dir: bool,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    /// Seconds since the Unix epoch.
    #[prost(int64, tag = "4")]
    pub modified: i64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileOpen {
    #[prost(string, tag = "1")]
    pub path: ::prost::alloc::string::String,
    #[prost(uint64, tag = "2")]
    pub size: u64,
    /// Byte offset to start from (resume).
    #[prost(uint64, tag = "3")]
    pub offset: u64,
    #[prost(bool, tag = "4")]
    pub overwrite: bool,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FileChunk {
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub data: ::prost::alloc::vec::Vec<u8>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TerminalMsg {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "terminal_msg::Op", tags = "2, 3, 4, 5")]
    pub op: ::core::option::Option<terminal_msg::Op>,
}

pub mod terminal_msg {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Op {
        #[prost(message, tag = "2")]
        Open(super::TerminalSize),
        #[prost(bytes = "vec", tag = "3")]
        Data(::prost::alloc::vec::Vec<u8>),
        #[prost(message, tag = "4")]
        Resize(super::TerminalSize),
        /// Exit code, or -1 when unknown.
        #[prost(sint32, tag = "5")]
        Exit(i32),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TerminalSize {
    #[prost(uint32, tag = "1")]
    pub cols: u32,
    #[prost(uint32, tag = "2")]
    pub rows: u32,
}

/// TCP port forwarding from the client to an address reachable by the host.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct TunnelMsg {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "tunnel_msg::Op", tags = "2, 3, 4, 5")]
    pub op: ::core::option::Option<tunnel_msg::Op>,
}

pub mod tunnel_msg {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Op {
        /// `host:port` the host should connect to.
        #[prost(string, tag = "2")]
        Open(::prost::alloc::string::String),
        #[prost(bool, tag = "3")]
        Opened(bool),
        #[prost(bytes = "vec", tag = "4")]
        Data(::prost::alloc::vec::Vec<u8>),
        #[prost(bool, tag = "5")]
        Closed(bool),
    }
}

/// Receiver feedback for congestion control.
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct Stats {
    #[prost(uint32, tag = "1")]
    pub rtt_ms: u32,
    #[prost(uint32, tag = "2")]
    pub decoded_fps: u32,
    #[prost(uint32, tag = "3")]
    pub dropped_frames: u32,
}
