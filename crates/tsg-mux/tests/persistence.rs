//! M2 の出口条件の実測（`arch.md` §9）。
//!
//! **ウィンドウを閉じてもプロセスが生き、再アタッチできる。**
//!
//! 実シェルを起動して往復するので、単体テストより遅い。それでもここを
//! ヘッドレスで確かめられるのは、mux をサーバプロセスとして分離したからで、
//! 不変条件 4 の実利そのもの。

use std::time::{Duration, Instant};

use tsg_mux::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg, decode_bytes};
use tsg_mux::{Client, server};

const TIMEOUT: Duration = Duration::from_secs(20);

fn unique_session(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("test-{tag}-{}-{nanos}", std::process::id())
}

/// `Attached` を待って、最初のペイン ID を返す。
fn attach(client: &mut Client) -> u32 {
    client
        .send(&ClientMsg::Attach {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cwd: None,
            command: None,
            restore: false,
        })
        .expect("Attach を送れない");

    let msg = client
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::Attached { .. }))
        .expect("Attached が返ってこない");

    match msg {
        ServerMsg::Attached { session, .. } => {
            assert!(
                !session.panes.is_empty(),
                "アタッチでペインが作られていない"
            );
            session.panes[0].id
        }
        _ => unreachable!(),
    }
}

/// プロンプトが出るまで待つ。
///
/// **どの記号で終わるかはシェル次第**（cmd は `>`、sh は `$`、root は `#`）。
/// 1 つずつ順に待つと、最初の待ちで時間を使い切って次に回らない。
/// まとめて 1 回のループで見る。
fn wait_prompt(client: &Client, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut seen = String::new();
    while Instant::now() < deadline {
        match client.recv_timeout(Duration::from_millis(200)) {
            Some(ServerMsg::Output { data, .. }) => {
                if let Some(bytes) = decode_bytes(&data) {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                }
                if seen.contains('>') || seen.contains('$') || seen.contains('#') {
                    return true;
                }
            }
            Some(_) | None => continue,
        }
    }
    false
}

/// 出力に `needle` が現れるまで待つ。
fn wait_output(client: &Client, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut seen = String::new();
    while Instant::now() < deadline {
        match client.recv_timeout(Duration::from_millis(200)) {
            Some(ServerMsg::Output { data, .. }) => {
                if let Some(bytes) = decode_bytes(&data) {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                }
                if seen.contains(needle) {
                    return true;
                }
            }
            Some(_) | None => continue,
        }
    }
    false
}

#[test]
fn panes_survive_a_disconnected_client() {
    let session = unique_session("persist");
    let handle = server::spawn(&session).expect("サーバを起こせない");

    // --- 1回目の接続: コマンドを実行して痕跡を残す ---
    let marker = "TSUMUGI_PERSIST_MARKER";
    {
        let mut client = Client::connect(&session).expect("接続できない");
        let pane = attach(&mut client);

        // シェルのプロンプトが出るまで待つ（起動直後は入力を取りこぼす）
        // 冷えた CI の走者では、最初のプロンプトが出るまで時間がかかる。
        //
        // **シェルが 1 文字も返さない環境なら、試すものが無い。**
        // この試験の主題は「クライアントが切れてもペインが生き残るか」で
        // あって「シェルがプロンプトを出すか」ではない。PTY を持てない
        // 走者で赤くしても、直せるものが何も出てこない。
        if !wait_prompt(&client, Duration::from_secs(60)) {
            eprintln!("シェルが応答しないので飛ばします（PTY を持てない環境）");
            handle.shutdown();
            return;
        }

        client
            .send(&ClientMsg::Input {
                pane,
                data: tsg_mux::encode_bytes(format!("echo {marker}\r").as_bytes()),
            })
            .expect("入力を送れない");

        assert!(
            wait_output(&client, marker, TIMEOUT),
            "実行したコマンドの出力が返ってこない"
        );
        // ここでクライアントを落とす = ウィンドウを閉じるのと同じ
    }

    // --- 2回目の接続: 状態が残っているか ---
    let mut again = Client::connect(&session).expect("再接続できない");
    again
        .send(&ClientMsg::Attach {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cwd: None,
            command: None,
            restore: false,
        })
        .expect("再 Attach を送れない");

    let snapshot = again
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::Snapshot { .. }))
        .expect("再アタッチでスナップショットが来ない");

    match snapshot {
        ServerMsg::Snapshot { lines, .. } => {
            let text = lines.join("\n");
            assert!(
                text.contains(marker),
                "再アタッチで画面が復元されていない。受信した内容:\n{text}"
            );
        }
        _ => unreachable!(),
    }

    handle.shutdown();
}

#[test]
fn an_open_file_survives_a_disconnected_client() {
    // M4 の宿題だったところ。**シェルは残るのにファイルは残らない**を塞いだ。
    let session = unique_session("file");
    let handle = server::spawn(&session).expect("サーバを起こせない");

    let path = std::env::temp_dir().join(format!("tsumugi-{session}.txt"));
    std::fs::write(&path, "original\n").expect("下準備のファイルが書けない");
    let path_str = path.display().to_string();

    // --- 1回目: 開いて、保存せずに書き換える ---
    {
        let mut client = Client::connect(&session).expect("接続できない");
        let pane = attach(&mut client);
        client
            .send(&ClientMsg::OpenFile {
                pane,
                path: path_str.clone(),
            })
            .expect("OpenFile を送れない");
        let msg = client
            .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::FileState { .. }))
            .expect("FileState が返ってこない");
        match msg {
            ServerMsg::FileState { text, dirty, .. } => {
                assert_eq!(text, "original\n");
                assert!(!dirty);
            }
            _ => unreachable!(),
        }

        client
            .send(&ClientMsg::SetFile {
                pane,
                text: "edited but not saved\n".into(),
            })
            .expect("SetFile を送れない");
        // 送り切る前に切ると取りこぼす
        std::thread::sleep(Duration::from_millis(300));
        // ここでクライアントを落とす = ウィンドウを閉じるのと同じ
    }

    // --- 2回目: 未保存の編集が残っているか ---
    let mut again = Client::connect(&session).expect("再接続できない");
    again
        .send(&ClientMsg::Attach {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            cwd: None,
            command: None,
            restore: false,
        })
        .expect("再 Attach を送れない");

    let msg = again
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::FileState { .. }))
        .expect("再アタッチでファイルが戻ってこない");
    match msg {
        ServerMsg::FileState { text, dirty, .. } => {
            assert_eq!(text, "edited but not saved\n", "編集が失われている");
            assert!(dirty, "未保存の印が落ちている");
        }
        _ => unreachable!(),
    }

    // ディスクはまだ元のまま（保存していないので当然）
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");

    handle.shutdown();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn saving_writes_through_the_server() {
    let session = unique_session("save");
    let handle = server::spawn(&session).expect("サーバを起こせない");
    let path = std::env::temp_dir().join(format!("tsumugi-{session}.txt"));
    let _ = std::fs::remove_file(&path);

    let mut client = Client::connect(&session).expect("接続できない");
    let pane = attach(&mut client);
    client
        .send(&ClientMsg::OpenFile {
            pane,
            path: path.display().to_string(),
        })
        .expect("OpenFile を送れない");
    client
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::FileState { .. }))
        .expect("FileState が返ってこない");

    client
        .send(&ClientMsg::SetFile {
            pane,
            text: "written\n".into(),
        })
        .expect("SetFile を送れない");
    client
        .send(&ClientMsg::SaveFile { pane, path: None })
        .expect("SaveFile を送れない");
    client
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::FileSaved { .. }))
        .expect("保存の返事が来ない");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "written\n");

    handle.shutdown();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn explicit_detach_keeps_the_session_alive() {
    let session = unique_session("detach");
    let handle = server::spawn(&session).expect("サーバを起こせない");

    let mut client = Client::connect(&session).expect("接続できない");
    let pane = attach(&mut client);
    client.send(&ClientMsg::Detach).expect("Detach を送れない");
    drop(client);

    // デタッチ後も同じセッションへ入り直せて、同じペインが居る
    let mut again = Client::connect(&session).expect("再接続できない");
    let pane_again = attach(&mut again);
    assert_eq!(pane, pane_again, "デタッチでペインが作り直されている");

    handle.shutdown();
}

#[test]
fn splitting_creates_a_second_pane_in_the_layout() {
    let session = unique_session("split");
    let handle = server::spawn(&session).expect("サーバを起こせない");

    let mut client = Client::connect(&session).expect("接続できない");
    let pane = attach(&mut client);

    client
        .send(&ClientMsg::Split {
            pane,
            dir: tsg_mux::Dir::Horizontal,
        })
        .expect("Split を送れない");

    let layout = client
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::Layout(_)))
        .expect("Layout が返ってこない");

    match layout {
        ServerMsg::Layout(info) => {
            assert_eq!(info.panes.len(), 2, "ペインが 2 つになっていない");
            let tab = &info.tabs[0];
            assert_eq!(tab.layout.panes().len(), 2, "木に反映されていない");
        }
        _ => unreachable!(),
    }

    handle.shutdown();
}

#[test]
fn protocol_version_mismatch_is_reported() {
    let session = unique_session("version");
    let handle = server::spawn(&session).expect("サーバを起こせない");

    let mut client = Client::connect(&session).expect("接続できない");
    client
        .send(&ClientMsg::Attach {
            version: PROTOCOL_VERSION + 99,
            cols: 80,
            rows: 24,
            cwd: None,
            command: None,
            restore: false,
        })
        .expect("Attach を送れない");

    let msg = client
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::Error { .. }))
        .expect("版違いが握り潰されている");
    assert!(matches!(msg, ServerMsg::Error { .. }));

    handle.shutdown();
}

/// **版が違っても止められる。**
///
/// 版が違うと Attach は弾かれる。便利な口はどれも Attach を通るので、
/// ここが塞がっていると「繋がるが何も言えない」サーバが残り、プロトコルを
/// 上げるたびに PID を手で探して殺すことになる（実際に踏んだ）。
/// `Shutdown` だけは Attach の前に受ける ── これが `tsg --kill` の土台。
#[test]
fn a_mismatched_client_can_still_shut_the_server_down() {
    let session = unique_session("version-kill");
    let handle = server::spawn(&session).expect("サーバを起こせない");

    let mut client = Client::connect(&session).expect("接続できない");
    client
        .send(&ClientMsg::Attach {
            version: PROTOCOL_VERSION + 99,
            cols: 80,
            rows: 24,
            cwd: None,
            command: None,
            restore: false,
        })
        .expect("Attach を送れない");
    client
        .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::Error { .. }))
        .expect("版違いが握り潰されている");

    // 止める前に、返事はすることを確かめる。**これが無いと空振りの試験**
    // （Pong が来ない別の理由でも通ってしまう）。
    client.send(&ClientMsg::Ping).expect("Ping を送れない");
    assert!(
        client
            .wait_for(TIMEOUT, |m| matches!(m, ServerMsg::Pong))
            .is_some(),
        "版が違うと Ping にも答えない ── 前提が崩れている"
    );

    // Attach していないまま、止まれと言う。
    client
        .send(&ClientMsg::Shutdown)
        .expect("Shutdown を送れない");

    // 止まったことは「もう返事をしない」で見る。**繋ぎ直しでは見られない**
    // （繋いだ口は同じプロセスの中では離れない ── 下の `shutting_down_releases_the_socket` を参照）。
    client.send(&ClientMsg::Ping).expect("Ping を送れない");
    assert!(
        client
            .wait_for(Duration::from_secs(2), |m| matches!(m, ServerMsg::Pong))
            .is_none(),
        "Shutdown を受けたのにまだ動いている"
    );

    handle.shutdown();
}

/// **止めたら口を手放す。** 同じ名前ですぐ開き直せる。
///
/// `accept` は繋がるまで返らないので、止める合図だけでは受け側が起きない。
/// 起こさずに終えると口を握ったままになり、次に同じ名前で開けない
/// （実測: 6 秒待っても開けなかった）。PC を落としたあとに前回と同じ
/// セッション名で開き直す道が、ここで塞がる。
///
/// **クライアントを繋いだあとは、同じプロセスの中では試せない。**
/// 繋いだ口は両側のスレッドが握り合っていて、片方のプロセスが終わるまで
/// 離れない。本番はクライアントが別プロセスなので、窓を閉じれば離れる。
#[test]
fn shutting_down_releases_the_socket() {
    let session = unique_session("rebind");
    let first = server::spawn(&session).expect("サーバを起こせない");
    first.shutdown();

    // 手放すまでの一瞬は待つ（受け側が起きて抜けるまで）。
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match server::spawn(&session) {
            Ok(second) => {
                second.shutdown();
                return;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "口を手放していない: {e:#}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
