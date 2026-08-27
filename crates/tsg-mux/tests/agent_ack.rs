//! 「返事待ち」の印は、応えたら下りる。
//!
//! 印を消せるのがエージェントの次の名乗りだけだと、実際には消えない。
//! `Stop` フックが毎ターン `done` を名乗るので印は既定で点いており、
//! 消すのは `UserPromptSubmit`（次のプロンプト）だけ — **動いている最中に
//! 打ち込んだ言葉はそのフックを通らない**。長い 1 ターンの途中で点いた印は
//! そのターンの間ずっと残る。
//!
//! ここで見ているのはエージェントの状態ではなく**人の動き**なので、
//! 状態（`agent`）は上書きせず、印（`agent_acked`）だけを下ろす。

use std::time::Duration;

use tsg_mux::protocol::{AgentState, ClientMsg, PROTOCOL_VERSION, ServerMsg};
use tsg_mux::{Client, server};

const TIMEOUT: Duration = Duration::from_secs(20);

fn unique_session(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("test-{tag}-{}-{nanos}", std::process::id())
}

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
        ServerMsg::Attached { session, .. } => session.panes[0].id,
        _ => unreachable!(),
    }
}

/// そのペインの (状態, 応えたか) が期待どおりの `Layout` が来るまで待つ。
///
/// **`Layout` は他の理由でも飛んでくる**（大きさ・タブ）ので、
/// 最初の 1 通で決めつけない。
fn wait_pane(
    client: &Client,
    pane: u32,
    want: (Option<AgentState>, bool),
    what: &str,
) -> (Option<AgentState>, bool) {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut last = (None, false);
    while std::time::Instant::now() < deadline {
        if let Some(ServerMsg::Layout(info)) = client.recv_timeout(Duration::from_millis(200))
            && let Some(p) = info.panes.iter().find(|p| p.id == pane)
        {
            last = (p.agent, p.agent_acked);
            if last == want {
                return last;
            }
        }
    }
    panic!("{what}: {want:?} を待ったが {last:?} のままだった");
}

#[test]
fn typing_into_a_pane_takes_the_waiting_mark_down() {
    let session = unique_session("ack");
    let handle = server::spawn(&session).expect("サーバを起こせない");
    let mut client = Client::connect(&session).expect("接続できない");
    let pane = attach(&mut client);

    // エージェントが「返事待ち」を名乗る。
    client
        .send(&ClientMsg::SetAgentState {
            pane: Some(pane),
            state: AgentState::Blocked,
            cost: None,
            agent: Some("claude".into()),
        })
        .expect("状態を送れない");
    wait_pane(
        &client,
        pane,
        (Some(AgentState::Blocked), false),
        "名乗った直後は印が点いている",
    );

    // 人がそのペインへ打ち込む＝応えた。
    client
        .send(&ClientMsg::Input {
            pane,
            data: tsg_mux::encode_bytes(b"y"),
        })
        .expect("入力を送れない");
    let (state, _) = wait_pane(
        &client,
        pane,
        (Some(AgentState::Blocked), true),
        "打ち込んだら印が下りる",
    );
    // **状態そのものは残す。** `--agents` や `--wait` が見るのはこちら。
    assert_eq!(
        state,
        Some(AgentState::Blocked),
        "印を下ろすために状態まで書き換えている"
    );

    // 同じ状態をもう一度名乗ってきたら、二度目も呼んでいる。
    client
        .send(&ClientMsg::SetAgentState {
            pane: Some(pane),
            state: AgentState::Blocked,
            cost: None,
            agent: None,
        })
        .expect("状態を送れない");
    wait_pane(
        &client,
        pane,
        (Some(AgentState::Blocked), false),
        "名乗り直したら印はまた点く",
    );

    handle.shutdown();
}

/// 応えていないペインの印は、隣に打っても下りない。
#[test]
fn answering_one_pane_leaves_the_other_alone() {
    let session = unique_session("ack2");
    let handle = server::spawn(&session).expect("サーバを起こせない");
    let mut client = Client::connect(&session).expect("接続できない");
    let a = attach(&mut client);

    client
        .send(&ClientMsg::Split {
            pane: a,
            dir: tsg_mux::protocol::Dir::Horizontal,
        })
        .expect("割れない");
    let b = {
        let deadline = std::time::Instant::now() + TIMEOUT;
        let mut found = None;
        while std::time::Instant::now() < deadline && found.is_none() {
            if let Some(ServerMsg::Layout(info)) = client.recv_timeout(Duration::from_millis(200)) {
                found = info.panes.iter().map(|p| p.id).find(|id| *id != a);
            }
        }
        found.expect("2 枚目のペインができない")
    };

    for id in [a, b] {
        client
            .send(&ClientMsg::SetAgentState {
                pane: Some(id),
                state: AgentState::Blocked,
                cost: None,
                agent: Some("claude".into()),
            })
            .expect("状態を送れない");
    }
    wait_pane(
        &client,
        b,
        (Some(AgentState::Blocked), false),
        "2 枚とも点く",
    );

    // a にだけ打つ。
    client
        .send(&ClientMsg::Input {
            pane: a,
            data: tsg_mux::encode_bytes(b"y"),
        })
        .expect("入力を送れない");
    wait_pane(&client, a, (Some(AgentState::Blocked), true), "a は下りる");

    // b はそのまま。**まとめて消すと、返事したのが 1 本でも全部消える。**
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if let Some(ServerMsg::Layout(info)) = client.recv_timeout(Duration::from_millis(200))
            && let Some(p) = info.panes.iter().find(|p| p.id == b)
        {
            assert!(!p.agent_acked, "打っていないペインの印まで下ろしている");
        }
    }

    handle.shutdown();
}
