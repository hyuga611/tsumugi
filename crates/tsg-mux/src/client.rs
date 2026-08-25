//! mux クライアント。GUI（`tsg`）はこれ越しにしかセッションを触らない。
//!
//! # 繋ぎ方は 2 つある
//!
//! 手元のセッションは名前付きソケット。遠隔のセッションは `ssh` の
//! パイプ越しに、向こうで走らせた `tsg --rpc` と話す。**話す言葉は同じ**
//! （JSON Lines）なので、変わるのは配管だけで、この上に載っているものは
//! どちらも知らずに済む。

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use interprocess::TryClone;

use crate::endpoint::Endpoint;
use crate::protocol::*;

/// 送る側の配管。
///
/// **遠隔のときは子プロセスを抱える。** 落としたら向こうとの線が切れるので、
/// クライアントが生きている間は持っておく。
enum Sink {
    Local(interprocess::local_socket::Stream),
    Remote {
        stdin: std::process::ChildStdin,
        child: std::process::Child,
    },
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(s) => s.write(buf),
            Self::Remote { stdin, .. } => stdin.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Local(s) => s.flush(),
            Self::Remote { stdin, .. } => stdin.flush(),
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        // 遠隔の子は自分で終わらないことがある。線を切ったら片付ける。
        if let Self::Remote { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub struct Client {
    writer: Sink,
    rx: Receiver<ServerMsg>,
    pub session: String,
}

impl Client {
    /// 走っているサーバへ繋ぐ。居なければエラー。
    ///
    /// 繋ぎ先が**自分のものであること**の確認は `Endpoint::connect` が持つ。
    /// ここでそれを迂回する経路を作らない。
    pub fn connect(session: &str) -> Result<Self> {
        let stream = Endpoint::for_session(session)?.connect()?;
        let writer = stream.try_clone().context("ソケットの複製に失敗")?;
        let rx = Self::pump(stream);
        Ok(Self {
            writer: Sink::Local(writer),
            rx,
            session: session.to_string(),
        })
    }

    /// 遠隔のセッションへ繋ぐ。
    ///
    /// 向こうで `tsg --rpc` を走らせて、その標準入出力を配管にする。
    /// **鍵も設定も `ssh` に任せる。** こちらで持つと、`~/.ssh/config` に
    /// 書いてあることを二重に書かせることになる。
    ///
    /// `program` は向こうの `tsg` の在り処（PATH に居るなら `"tsg"`）。
    /// `ssh` は繋ぐのに使うもの（既定は `"ssh"`）。
    pub fn over_ssh(ssh: &str, target: &str, program: &str, session: &str) -> Result<Self> {
        if target.trim().is_empty() {
            bail!("繋ぎ先がありません");
        }
        let mut cmd = std::process::Command::new(ssh);
        cmd.args(ssh_args(target, program, session));
        // 向こうの警告（鍵の指紋など）は人が読めるところへ出す。
        cmd.stderr(std::process::Stdio::inherit());
        Self::over_pipe(cmd, session).with_context(|| format!("{target} へ繋げません"))
    }

    /// 子プロセスの標準入出力を配管にして繋ぐ。
    ///
    /// **`ssh` に限らない。** 話す言葉は同じなので、向こうと繋がる
    /// コマンドでありさえすればいい。
    pub fn over_pipe(mut cmd: std::process::Command, session: &str) -> Result<Self> {
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().context("起動できません")?;
        let stdin = child.stdin.take().context("入力を掴めません")?;
        let stdout = child.stdout.take().context("出力を掴めません")?;
        let rx = Self::pump(stdout);
        Ok(Self {
            writer: Sink::Remote { stdin, child },
            rx,
            session: session.to_string(),
        })
    }

    /// 読む係。行ごとに解いて流す。
    fn pump(source: impl Read + Send + 'static) -> Receiver<ServerMsg> {
        let (tx, rx) = mpsc::channel::<ServerMsg>();
        thread::spawn(move || {
            let reader = BufReader::new(source);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<ServerMsg>(&line)
                    && tx.send(msg).is_err()
                {
                    break;
                }
            }
        });
        rx
    }

    pub fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let line = serde_json::to_string(msg)?;
        writeln!(self.writer, "{line}")?;
        self.writer.flush()?;
        Ok(())
    }

    /// 来ていれば 1 通。**切れているかどうかはここでは区別しない**
    /// （切れたことは他の道で分かるし、呼ぶ側は「今は無い」と同じに扱う）。
    pub fn try_recv(&self) -> Option<ServerMsg> {
        self.rx.try_recv().ok()
    }

    pub fn recv_timeout(&self, dur: Duration) -> Option<ServerMsg> {
        self.rx.recv_timeout(dur).ok()
    }

    /// 条件を満たすメッセージが来るまで待つ（テストと初期化用）。
    pub fn wait_for(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&ServerMsg) -> bool,
    ) -> Option<ServerMsg> {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.recv_timeout(left.min(Duration::from_millis(200))) {
                Some(msg) if pred(&msg) => return Some(msg),
                Some(_) => continue,
                None => continue,
            }
        }
        None
    }
}

/// `ssh` に渡す引数。
///
/// `-T`: 向こうで端末を割り当てない。**欲しいのはバイトの通り道で、
/// 画面ではない。** 割り当てると改行が化けて JSON が壊れる。
///
/// `--spawn`: 向こうにまだ誰も居なければ起こす。手元の `--rpc` は
/// 起こさない（覗くだけの口）ので、ここで明示する。
fn ssh_args(target: &str, program: &str, session: &str) -> Vec<String> {
    [
        "-T",
        target,
        program,
        "--rpc",
        "--spawn",
        "--session",
        session,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 向こうで走らせる形。**端末を割り当てない**のと、
    /// **居なければ起こす**のが要点。
    #[test]
    fn the_far_end_is_asked_for_a_pipe_not_a_screen() {
        let args = ssh_args("user@host", "tsg", "work");
        assert_eq!(
            args,
            vec![
                "-T",
                "user@host",
                "tsg",
                "--rpc",
                "--spawn",
                "--session",
                "work"
            ]
        );
    }

    /// 繋ぎ先が空なら、`ssh` を起こす前に断る。
    #[test]
    fn an_empty_target_is_refused_before_spawning_anything() {
        assert!(Client::over_ssh("ssh", "   ", "tsg", "work").is_err());
    }
}
