//! モーダル操作の中核。**I/O 依存ゼロ**（`arch.md` の不変条件 2）。
//!
//! - `command` — コマンド語彙とレジストリ。マウス等価を CI で保証する土台
//! - `motion`  — `modal-spec.md` §5.1 の汎用モーション
//! - `textobj` — `modal-spec.md` §6 のテキストオブジェクト（マウスの二度押しもここを通る）
//! - `engine`  — モード機械と唯一のディスパッチャ
//!
//! ホスト（`tsg`）はキーを `KeyInput` に翻訳して `Engine::key` へ渡し、
//! 返ってきた `Effect` を実行するだけでよい。

pub mod command;
pub mod engine;
pub mod format;
pub mod keymap;
mod motion;
mod search;
pub mod text;
pub mod textobj;

pub use command::{
    Command, CommandSpec, FileAction, HistoryAction, InsertAt, Mode, MousePath, REGISTRY,
    VisualKind,
};
pub use engine::{Effect, Engine, KeyInput, KeyOutcome, Macros, Marks, RegisterValue, Registers};
pub use keymap::{Keymap, When as KeyWhen, parse_key};
pub use motion::{Motion, MotionKind, View, find_match, matches_in};
pub use search::Search;
pub use text::{Lang, lang, set_lang};
pub use textobj::TextObject;

pub use tsg_buffer::markdown;
pub use tsg_buffer::{
    Buffer, BufferKind, FileBuffer, Lang as SyntaxLang, OperatorId, Pos, Range, RangeKind, Splice,
    SyntaxState, TermBuffer, Token, clamp_insert, extract, highlight, highlight_from, line_text,
};
