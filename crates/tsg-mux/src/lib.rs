//! セッション / タブ / ペインの多重化と永続化。
//!
//! `arch.md` の不変条件 4「mux は常に別プロセス」。
//! ウィンドウを閉じてもプロセスが生き、再アタッチできることがこの層の存在理由。

pub mod client;
pub mod protocol;
pub mod server;
pub mod sessions;

pub use client::Client;
pub use protocol::{
    ClientMsg, Dir, Edit, Layout, PaneInfo, PROTOCOL_VERSION, ServerMsg, SessionInfo, TabInfo,
    decode_bytes, encode_bytes, socket_name,
};
pub use server::{ServerHandle, run, spawn};
