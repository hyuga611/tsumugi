//! PTY 抽象。ConPTY / Unix PTY の差を吸収する薄い層。
//!
//! `arch.md` §2 の通り、ConPTY は自作しない。`portable-pty` に寄せる。
//! このクレートの責務は「プロセスを起こす・生バイトを流す・大きさを変える」だけで、
//! エスケープシーケンスの意味づけは `tsg-term` が持つ。

use std::io::{Read, Write};

use anyhow::{Context, Result};
use portable_pty::{Child, MasterPty, native_pty_system};
pub use portable_pty::{CommandBuilder, ExitStatus, PtySize};

/// 起動済みの PTY と、その上で走る子プロセス。
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtySession {
    /// コマンドを PTY 上で起動する。
    pub fn spawn(cmd: CommandBuilder, size: PtySize) -> Result<Self> {
        let pty = native_pty_system();
        let pair = pty.openpty(size).context("PTY の確保に失敗")?;

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("子プロセスの起動に失敗")?;

        // slave 側を確実に手放す。保持したままだと、子プロセスが終了しても
        // reader が EOF を返さず読み取りが永久にブロックする。
        drop(pair.slave);

        Ok(Self {
            master: pair.master,
            child,
        })
    }

    /// 出力の読み取り口。複数回呼べる（それぞれ独立したハンドル）。
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master
            .try_clone_reader()
            .context("PTY の読み取り口の複製に失敗")
    }

    /// 入力の書き込み口。`take_writer` の名の通り、実質一度だけ取る想定。
    pub fn writer(&self) -> Result<Box<dyn Write + Send>> {
        self.master
            .take_writer()
            .context("PTY の書き込み口の取得に失敗")
    }

    pub fn resize(&self, size: PtySize) -> Result<()> {
        self.master.resize(size).context("PTY のリサイズに失敗")
    }

    /// 子プロセスの終了を待つ。
    pub fn wait(&mut self) -> Result<ExitStatus> {
        self.child.wait().context("子プロセスの待機に失敗")
    }

    /// 終了していれば終了ステータスを返す。走っていれば `None`。
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child.try_wait().context("子プロセスの状態取得に失敗")
    }

    pub fn kill(&mut self) -> Result<()> {
        self.child.kill().context("子プロセスの終了に失敗")
    }
}

/// 既定のウィンドウサイズ。
pub fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
