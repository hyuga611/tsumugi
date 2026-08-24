//! mux クライアント。GUI（`tsg`）はこれ越しにしかセッションを触らない。

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use interprocess::TryClone;
use interprocess::local_socket::Stream;

use crate::endpoint::Endpoint;
use crate::protocol::*;

pub struct Client {
    writer: Stream,
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

        let (tx, rx) = mpsc::channel::<ServerMsg>();
        thread::spawn(move || {
            let reader = BufReader::new(stream);
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

        Ok(Self {
            writer,
            rx,
            session: session.to_string(),
        })
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
