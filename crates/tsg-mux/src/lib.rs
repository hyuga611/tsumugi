//! セッション / タブ / ペインの多重化と永続化。
//!
//! `arch.md` の不変条件 4「mux は常に別プロセス」。
//! ウィンドウを閉じてもプロセスが生き、再アタッチできることがこの層の存在理由。

pub mod client;
pub mod endpoint;
pub mod protocol;
pub mod server;
pub mod sessions;
#[cfg(windows)]
mod win_sd;

pub use client::Client;
pub use endpoint::Endpoint;
pub use protocol::{
    ClientMsg, Dir, Edit, Layout, PROTOCOL_VERSION, PaneInfo, ServerMsg, SessionInfo, TabInfo,
    decode_bytes, encode_bytes,
};
pub use server::{ServerHandle, run, spawn};
