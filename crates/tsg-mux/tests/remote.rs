//! 遠隔の配管（`ssh` 越し）が、手元と同じ言葉で話せるか。
//!
//! **`ssh` そのものは試さない。** 試したいのは「パイプ越しでも同じ
//! プロトコルが通るか」で、鍵や向こうの環境ではない。`ssh` の代わりに
//! 同じ形で答える子プロセスを立てて、`Client::over_ssh` の配管だけを見る。

use std::io::{BufRead, Write};
use std::time::Duration;

use tsg_mux::Client;
use tsg_mux::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg, SessionInfo};

/// 「向こう側」のふり。`--rpc` と同じで、JSON Lines を読んで返す。
///
/// 試験の実行ファイル自身を、環境変数で答える側として立て直す。
/// **別の実行ファイルを用意しない**ので、CI でも同じように動く。
fn be_the_far_end() {
    let out = std::io::stdout();
    let mut out = out.lock();
    let hello = ServerMsg::Attached {
        version: PROTOCOL_VERSION,
        session: SessionInfo {
            name: "far".into(),
            tabs: Vec::new(),
            active_tab: 1,
            panes: Vec::new(),
        },
    };
    let _ = writeln!(out, "{}", serde_json::to_string(&hello).unwrap());
    let _ = out.flush();

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        // 受け取ったものをそのまま鏡にして返す（届いたことの証拠）。
        let echo = ServerMsg::Error { message: line };
        let _ = writeln!(out, "{}", serde_json::to_string(&echo).unwrap());
        let _ = out.flush();
    }
}

#[test]
fn the_protocol_survives_a_pipe() {
    if std::env::var("TSG_TEST_FAR_END").is_ok() {
        be_the_far_end();
        return;
    }
    // 子は自分自身。`--exact` でこの試験だけを走らせ、環境変数で
    // 「答える側」に切り替える。
    let exe = std::env::current_exe().expect("自分の場所が分からない");
    let exe = exe.display().to_string();
    unsafe { std::env::set_var("TSG_TEST_FAR_END", "1") };

    // `ssh` の代わりに子を立てる。`ssh` を起こすところは `over_ssh` の
    // 仕事で、そちらは引数の形として別に試す（`client.rs`）。
    // ここで見るのは**パイプ越しに同じ言葉が通るか**だけ。
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["--exact", "the_protocol_survives_a_pipe", "--nocapture"]);
    cmd.stderr(std::process::Stdio::null());
    let mut client = Client::over_pipe(cmd, "far").expect("向こう側を起こせない");

    // **試験の走者が先に出す行がある**（"running 1 test" など）。
    // 読めない行は捨てて進むので、そこで止まらない。挨拶が届けば通る。
    let hello = client.wait_for(Duration::from_secs(20), |m| {
        matches!(m, ServerMsg::Attached { .. })
    });
    assert!(hello.is_some(), "遠隔から Attached が返らない");

    // こちらから送ったものが、そのまま向こうへ届く。
    client.send(&ClientMsg::Ping).expect("送れない");
    let back = client
        .wait_for(Duration::from_secs(20), |m| {
            matches!(m, ServerMsg::Error { .. })
        })
        .expect("返事が無い");
    match back {
        ServerMsg::Error { message } => {
            let echoed: ClientMsg = serde_json::from_str(&message).expect("鏡が壊れている");
            assert_eq!(echoed, ClientMsg::Ping, "送ったものが届いていない");
        }
        other => panic!("違うものが返ってきた: {other:?}"),
    }
}
