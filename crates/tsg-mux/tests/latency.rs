//! `arch.md` §4 の判定ゲート。
//!
//! 不変条件 4「mux は常に別プロセス」は**レイテンシと引き換え**である。
//! ローカル 1 ウィンドウでも、全キー入力と全出力が IPC を通る。
//! 「入力 -> 画面反映が 8ms を超えるなら共有メモリの高速経路を足す」と決めたので、
//! その予算のうち **IPC が食う分**をここで測り、回帰したら落ちるようにする。
//!
//! 測るのは往復（クライアント -> state スレッド -> クライアント）。
//! 描画とシェルの応答は含まない。

use std::time::{Duration, Instant};

use tsg_mux::protocol::{ClientMsg, PROTOCOL_VERSION, ServerMsg};
use tsg_mux::{Client, server};

/// 8ms の予算のうち、IPC 往復に許す上限。
/// 残りは PTY・解析・描画のために空けておく。
const IPC_BUDGET: Duration = Duration::from_millis(4);

const SAMPLES: usize = 200;

#[test]
fn ipc_round_trip_stays_within_the_latency_budget() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let session = format!("test-latency-{}-{nanos}", std::process::id());

    let handle = server::spawn(&session).expect("サーバを起こせない");
    let mut client = Client::connect(&session).expect("接続できない");

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
    client
        .wait_for(Duration::from_secs(20), |m| {
            matches!(m, ServerMsg::Attached { .. })
        })
        .expect("Attached が返ってこない");

    // シェル起動直後の出力で計測が汚れないよう、少し流してから測る。
    std::thread::sleep(Duration::from_millis(500));
    while client.try_recv().is_some() {}

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        // 計測前に溜まった出力を捨てる（シェルが喋り続けている場合の保険）
        while client.try_recv().is_some() {}

        let start = Instant::now();
        client.send(&ClientMsg::Ping).expect("Ping を送れない");
        let got = client.wait_for(Duration::from_secs(5), |m| matches!(m, ServerMsg::Pong));
        let elapsed = start.elapsed();
        assert!(got.is_some(), "Pong が返ってこない");
        samples.push(elapsed);
    }

    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];
    let worst = *samples.last().unwrap();

    println!("IPC 往復レイテンシ（{SAMPLES} 回）");
    println!("  中央値 {:>8.3} ms", median.as_secs_f64() * 1000.0);
    println!("  p95    {:>8.3} ms", p95.as_secs_f64() * 1000.0);
    println!("  最悪   {:>8.3} ms", worst.as_secs_f64() * 1000.0);
    println!(
        "  予算   {:>8.3} ms（8ms のうち IPC に割く分）",
        IPC_BUDGET.as_secs_f64() * 1000.0
    );

    handle.shutdown();

    assert!(
        median <= IPC_BUDGET,
        "IPC 往復の中央値が予算を超えた: {:.3} ms > {:.3} ms。\n\
         arch.md の不変条件 4 の通り、グリッド転送に共有メモリの高速経路を足すか、\n\
         プロトコルを msgpack へ移す判断が要る。",
        median.as_secs_f64() * 1000.0,
        IPC_BUDGET.as_secs_f64() * 1000.0
    );
}
