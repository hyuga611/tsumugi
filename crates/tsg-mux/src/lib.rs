//! セッション / タブ / ペインの多重化と永続化。
//!
//! `arch.md` の不変条件 4「mux は常に別プロセス」。
//! ウィンドウを閉じてもプロセスが生き、再アタッチできることがこの層の存在理由。

pub mod client;
pub mod endpoint;
pub mod lsp;
pub mod protocol;
pub mod restore;
pub mod server;
pub mod sessions;
#[cfg(windows)]
mod win_sd;

pub use client::Client;
pub use endpoint::Endpoint;
pub use protocol::{
    ClientMsg, Dir, Edit, ExtCommand, ExtLogEntry, Layout, LayoutSpec, Level, Match, MatchInput,
    PROTOCOL_VERSION, PaneInfo, PluginEvent, ServerMsg, SessionInfo, TabInfo, decode_bytes,
    encode_bytes,
};
pub use server::{ServerHandle, run_with, spawn};
