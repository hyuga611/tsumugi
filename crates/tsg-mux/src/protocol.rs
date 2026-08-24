//! クライアント ⇔ サーバのプロトコル。JSON Lines（1行1メッセージ）。
//!
//! `arch.md` §2 の通り、デバッグ可能・外部からスクリプト可能を最優先にした。
//! 性能が問題になれば msgpack へ差し替える（形は変えない）。
//!
//! 設計上の要点: **グリッドの差分ではなく PTY の生バイトを送る。**
//! クライアントは自前の `tsg-term` で解析して自分のグリッドを作る。
//! これで差分の直列化を実装せずに済み、プロトコルが端末仕様に引きずられない。
//! 再アタッチのときだけ、サーバが持つグリッドをスナップショットとして送る。

use base64::prelude::{BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 7;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// 左右に分割
    Horizontal,
    /// 上下に分割
    Vertical,
}

/// ペインの木。`arch.md` の「セッション > タブ > ペインの木」。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Layout {
    Leaf {
        pane: u32,
    },
    Split {
        dir: Dir,
        children: Vec<Layout>,
        /// 子の取り分。合計に対する比で割り付ける。境界のドラッグはここを動かす。
        /// 長さが合わなければ均等に読み替える（`weights_for`）。
        #[serde(default)]
        weights: Vec<u16>,
    },
}

/// 割り付けの基準値。分割直後はどの子もこれを持つ。
pub const WEIGHT_UNIT: u16 = 100;

/// 子の取り分。壊れていれば均等として読む。
///
/// レイアウト木は再アタッチで往復するので、**壊れた重みで割り付けが崩れるより
/// 均等へ落ちる方が安全**という判断。
pub fn weights_for(children: &[Layout], weights: &[u16]) -> Vec<u16> {
    if weights.len() == children.len() && !weights.contains(&0) {
        weights.to_vec()
    } else {
        vec![WEIGHT_UNIT; children.len()]
    }
}

/// 子の数に合わせて重みの長さを揃える。
fn normalize(weights: &mut Vec<u16>, len: usize) {
    if weights.len() != len || weights.contains(&0) {
        *weights = vec![WEIGHT_UNIT; len];
    }
}

impl Layout {
    pub fn leaf(pane: u32) -> Self {
        Layout::Leaf { pane }
    }

    pub fn panes(&self) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<u32>) {
        match self {
            Layout::Leaf { pane } => out.push(*pane),
            Layout::Split { children, .. } => {
                for c in children {
                    c.collect(out);
                }
            }
        }
    }

    /// `target` の葉を分割して `new_pane` を隣に置く。
    pub fn split(&mut self, target: u32, new_pane: u32, dir: Dir) -> bool {
        match self {
            Layout::Leaf { pane } if *pane == target => {
                *self = Layout::Split {
                    dir,
                    children: vec![Layout::leaf(target), Layout::leaf(new_pane)],
                    weights: vec![WEIGHT_UNIT; 2],
                };
                true
            }
            Layout::Leaf { .. } => false,
            Layout::Split {
                dir: my_dir,
                children,
                weights,
            } => {
                // 同じ向きの分割なら、木を深くせず兄弟として差し込む。
                if *my_dir == dir
                    && let Some(i) = children
                        .iter()
                        .position(|c| matches!(c, Layout::Leaf { pane } if *pane == target))
                {
                    // 割った側の取り分を新しい子と分け合う（他の兄弟は動かさない）
                    normalize(weights, children.len());
                    let half = (weights[i] / 2).max(1);
                    weights[i] = weights[i].saturating_sub(half).max(1);
                    children.insert(i + 1, Layout::leaf(new_pane));
                    weights.insert(i + 1, half);
                    return true;
                }
                children.iter_mut().any(|c| c.split(target, new_pane, dir))
            }
        }
    }

    /// 葉を取り除く。子が1つになった分割は畳む。
    pub fn remove(&mut self, target: u32) -> bool {
        let Layout::Split {
            children, weights, ..
        } = self
        else {
            return false;
        };
        normalize(weights, children.len());

        // 重みは子と対で消す。片方だけ消すと以降の割り付けが1つずつずれる。
        let mut removed = false;
        let mut i = 0;
        while i < children.len() {
            if matches!(&children[i], Layout::Leaf { pane } if *pane == target) {
                children.remove(i);
                weights.remove(i);
                removed = true;
            } else {
                i += 1;
            }
        }
        if !removed {
            removed = children.iter_mut().any(|c| c.remove(target));
        }

        let mut i = 0;
        while i < children.len() {
            if matches!(&children[i], Layout::Split { children, .. } if children.is_empty()) {
                children.remove(i);
                weights.remove(i);
            } else {
                i += 1;
            }
        }
        if children.len() == 1 {
            *self = children.remove(0);
        }
        removed
    }

    /// `pane` の取り分を隣から `delta` だけ奪う。境界ドラッグの実体。
    ///
    /// **サーバのレイアウト木を動かす**ので、再アタッチしても分割比が戻らない。
    pub fn resize(&mut self, pane: u32, delta: i32) -> bool {
        let Layout::Split {
            children, weights, ..
        } = self
        else {
            return false;
        };
        normalize(weights, children.len());

        if let Some(i) = children
            .iter()
            .position(|c| matches!(c, Layout::Leaf { pane: p } if *p == pane))
        {
            // 末端のペインは右（下）に隣が無いので、左（上）の隣と取引する
            let Some(j) = (if i + 1 < children.len() {
                Some(i + 1)
            } else {
                i.checked_sub(1)
            }) else {
                return false;
            };
            let d = delta.clamp(-(i32::from(weights[i]) - 1), i32::from(weights[j]) - 1);
            if d == 0 {
                return false;
            }
            weights[i] = (i32::from(weights[i]) + d) as u16;
            weights[j] = (i32::from(weights[j]) - d) as u16;
            return true;
        }
        children.iter_mut().any(|c| c.resize(pane, delta))
    }

    /// 2 つの葉を入れ替える。木の形はそのままで、載っているペインだけが動く。
    ///
    /// 葉を持ち上げて差し替えるのではなく **id を差し替える**のは、
    /// 分割比（重み）を場所に紐づけたままにするため。入れ替えたときに
    /// 幅まで一緒に動くと、狭い方へ移したペインが勝手に縮む。
    pub fn swap(&mut self, a: u32, b: u32) -> bool {
        if a == b {
            return false;
        }
        let (mut found_a, mut found_b) = (false, false);
        self.visit_leaves(&mut |pane| {
            if *pane == a {
                found_a = true;
            } else if *pane == b {
                found_b = true;
            }
        });
        if !(found_a && found_b) {
            return false;
        }
        self.visit_leaves(&mut |pane| {
            if *pane == a {
                *pane = b;
            } else if *pane == b {
                *pane = a;
            }
        });
        true
    }

    /// 取り分を全部そろえる（`Space =`）。
    pub fn equalize(&mut self) {
        if let Layout::Split {
            children, weights, ..
        } = self
        {
            *weights = vec![WEIGHT_UNIT; children.len()];
            for c in children.iter_mut() {
                c.equalize();
            }
        }
    }

    fn visit_leaves(&mut self, f: &mut impl FnMut(&mut u32)) {
        match self {
            Layout::Leaf { pane } => f(pane),
            Layout::Split { children, .. } => {
                for c in children {
                    c.visit_leaves(f);
                }
            }
        }
    }
}

/// 1 つの置換。位置はファイル全文の上のバイト。
///
/// 消えた中身は送らない（サーバは持っているので長さだけで足りる）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub remove: usize,
    pub insert: String,
}

impl Edit {
    /// 文字列へ当てる。当てられなければ `false`（呼ぶ側が立て直す）。
    ///
    /// 文字境界を確かめるのは、ずれた位置へ当てると `String` が壊れて
    /// **パニックする**ため。壊れた状態で走り続けるより、当てずに気づく。
    pub fn apply(&self, text: &mut String) -> bool {
        let end = self.start.saturating_add(self.remove);
        if end > text.len() || !text.is_char_boundary(self.start) || !text.is_char_boundary(end) {
            return false;
        }
        text.replace_range(self.start..end, &self.insert);
        true
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub id: u32,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TabInfo {
    pub id: u32,
    pub layout: Layout,
    pub active_pane: u32,
    /// 1 枚だけを画面いっぱいに出しているとき、その id（`Space z`）。
    ///
    /// **木は畳まない。** 畳むと戻すときに元の形を復元する必要が出て、
    /// 分割比まで失う。出すものを選ぶだけにしてある。
    #[serde(default)]
    pub zoom: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub tabs: Vec<TabInfo>,
    pub active_tab: u32,
    pub panes: Vec<PaneInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Attach {
        version: u32,
        cols: u16,
        rows: u16,
        /// 最初のペインを開く作業ディレクトリ。
        ///
        /// 「ここでターミナルを開く」を成立させるために要る。サーバは常駐なので、
        /// サーバ自身の cwd を使うと**どこから起動しても同じ場所**で開いてしまう。
        #[serde(default)]
        cwd: Option<String>,
        /// シェルの代わりに走らせるもの（`-e`）。
        #[serde(default)]
        command: Option<Vec<String>>,
    },
    /// キー入力。`data` は base64。
    Input {
        pane: u32,
        data: String,
    },
    Resize {
        pane: u32,
        cols: u16,
        rows: u16,
    },
    Split {
        pane: u32,
        dir: Dir,
    },
    ClosePane {
        pane: u32,
    },
    /// 境界のドラッグ。`pane` の取り分を隣から `delta` だけ奪う。
    ResizeSplit {
        pane: u32,
        delta: i32,
    },
    /// 2 つのペインを入れ替える（`Space HJKL` / メニュー）。
    ///
    /// **どちらが隣かを決めるのはクライアント**。画面の幾何を知っているのは
    /// 割り付けを描いている側だけなので、サーバは言われた 2 枚を入れ替える。
    SwapPanes {
        a: u32,
        b: u32,
    },
    /// 分割比を全部そろえる（`Space =` / 境界のダブルクリック）。
    Equalize {
        tab: u32,
    },
    /// 1 枚だけを画面いっぱいに出す。`pane` が `None` なら戻す（`Space z`）。
    SetZoom {
        tab: u32,
        pane: Option<u32>,
    },
    /// タブの並べ替え（タブのドラッグ）。`to` は移動先の位置。
    MoveTab {
        tab: u32,
        to: usize,
    },

    // ---- ファイルバッファ ----
    //
    // 中身をサーバに持たせる。クライアント側だけで持つと、
    // **シェルは残るのにファイルは残らない**という非対称ができる。
    /// このペインでファイルを開く。`path` は解決済みの絶対パス。
    OpenFile {
        pane: u32,
        path: String,
    },
    /// 全文を渡し直す。取り消しの後と、ずれが見つかったときの立て直しに使う。
    SetFile {
        pane: u32,
        text: String,
    },
    /// 編集の差分。**普段の打鍵はこちらを送る。**
    ///
    /// `base_len` は「クライアントが思っているサーバ側の長さ」。食い違ったら
    /// 途中の通知を取りこぼしているので、当てずっぽうで当てにいかず
    /// `NeedFullFile` で全文を要求する。黙って当てると中身が静かに壊れる。
    EditFile {
        pane: u32,
        base_len: usize,
        edits: Vec<Edit>,
    },
    /// 保存する。`path` を渡すと保存先を決める（`:w <パス>`）。
    SaveFile {
        pane: u32,
        #[serde(default)]
        path: Option<String>,
    },
    CloseFile {
        pane: u32,
    },
    /// `>` の結果を新しいペインで開く。
    ///
    /// 分割とバッファ生成を 1 通で済ませる。2 通に分けると
    /// 「今できたペインの ID」をクライアントが追いかける必要が出て、
    /// 非同期の取りこぼしが入る余地ができる。
    PipeResult {
        pane: u32,
        dir: Dir,
        title: String,
        text: String,
    },
    NewTab,
    SelectTab {
        tab: u32,
    },
    /// 切断するがプロセスは生かす（これが永続性の本体）。
    Detach,
    /// サーバごと落とす。
    Shutdown,
    Ping,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Attached {
        version: u32,
        session: SessionInfo,
    },
    /// 再アタッチ時の画面復元。生バイトの再送ではなく確定した行を送る。
    ///
    /// 各行は **SGR 付きの ANSI**。クライアントは同じ `tsg-term` に食わせるので、
    /// 復元路のために別の描画コードを持たずに済む（版 2 からこの形）。
    Snapshot {
        pane: u32,
        lines: Vec<String>,
        cursor_line: usize,
        cursor_col: usize,
    },
    /// ペインの実サイズが変わった。
    ///
    /// **クライアントは自分の鏡を勝手にリサイズしてはいけない。**
    /// ConPTY が実際にサイズを変えるのはサーバが `pty.resize()` を呼んだ瞬間で、
    /// それ以前のバイトは古い桁数で組まれている。先に鏡を広げると、
    /// 古い桁で折り返された行を新しい桁で読み直して表示が崩れる。
    /// この通知はバイト列と同じ順序で届くので、受けた位置で合わせれば必ず一致する。
    Resized {
        pane: u32,
        cols: u16,
        rows: u16,
    },
    /// ファイルの現在の中身。開いたときと、再アタッチしたときに送る。
    ///
    /// **これが「閉じても編集が消えない」の本体。**
    FileState {
        pane: u32,
        /// 保存先。`>` の結果のように、まだ決まっていないこともある。
        path: Option<String>,
        /// 表示用の名前（保存先が無いときはこれだけが手がかりになる）。
        title: String,
        text: String,
        dirty: bool,
    },
    FileSaved {
        pane: u32,
        path: String,
    },
    FileClosed {
        pane: u32,
    },
    /// 差分を当てられなかった。クライアントは全文を送り直す。
    NeedFullFile {
        pane: u32,
    },
    /// PTY の生バイト（base64）。
    Output {
        pane: u32,
        data: String,
    },
    Layout(SessionInfo),
    PaneExited {
        pane: u32,
    },
    Pong,
    Error {
        message: String,
    },
}

pub fn encode_bytes(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(bytes)
}

pub fn decode_bytes(s: &str) -> Option<Vec<u8>> {
    BASE64_STANDARD.decode(s).ok()
}

/// このセッションのソケット名。
pub fn socket_name(session: &str) -> String {
    format!("tsumugi-{session}.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_that_does_not_fit_is_refused_instead_of_applied() {
        let mut text = String::from("hello");
        assert!(
            Edit {
                start: 1,
                remove: 2,
                insert: "X".into()
            }
            .apply(&mut text)
        );
        assert_eq!(text, "hXlo");

        // 範囲の外
        assert!(
            !Edit {
                start: 99,
                remove: 0,
                insert: "z".into()
            }
            .apply(&mut text)
        );
        assert_eq!(text, "hXlo", "当てられないのに書き換わっている");
    }

    #[test]
    fn an_edit_never_cuts_a_character_in_half() {
        let mut text = String::from("あい");
        assert!(
            !Edit {
                start: 1,
                remove: 1,
                insert: "".into()
            }
            .apply(&mut text),
            "全角の途中へ当てて壊れる"
        );
        assert_eq!(text, "あい");
    }

    #[test]
    fn swapping_moves_the_panes_and_leaves_the_widths_alone() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.resize(1, 40);
        let Layout::Split { weights, .. } = &l else {
            panic!("分割になっていない")
        };
        let before = weights.clone();

        assert!(l.swap(1, 2));
        assert_eq!(l.panes(), vec![2, 1], "中身が入れ替わっていない");
        let Layout::Split { weights, .. } = &l else {
            panic!()
        };
        assert_eq!(
            *weights, before,
            "入れ替えで幅まで動いた。狭い方へ移したペインが勝手に縮む"
        );
    }

    #[test]
    fn swapping_needs_both_panes_to_exist() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        assert!(!l.swap(1, 99), "居ないペインと入れ替わった");
        assert!(!l.swap(1, 1), "自分自身と入れ替えた");
        assert_eq!(l.panes(), vec![1, 2]);
    }

    #[test]
    fn equalizing_resets_every_level() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(2, 3, Dir::Vertical);
        l.resize(1, 30);
        l.equalize();

        fn all_equal(l: &Layout) -> bool {
            match l {
                Layout::Leaf { .. } => true,
                Layout::Split {
                    children, weights, ..
                } => {
                    weights.iter().all(|w| *w == WEIGHT_UNIT) && children.iter().all(all_equal)
                }
            }
        }
        assert!(all_equal(&l), "入れ子の下まで揃っていない");
    }

    #[test]
    fn splitting_shares_only_the_split_pane_weight() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(1, 3, Dir::Horizontal);
        let Layout::Split { weights, children, .. } = &l else {
            panic!("分割になっていない")
        };
        assert_eq!(children.len(), 3);
        assert_eq!(weights.len(), children.len(), "重みと子の数がずれている");
        // 割ったのは 1。無関係な 2 の取り分は動かない。
        assert_eq!(weights[2], WEIGHT_UNIT, "割っていないペインまで縮んでいる");
        assert_eq!(weights[0] + weights[1], WEIGHT_UNIT, "1 の取り分が増減している");
    }

    #[test]
    fn resize_moves_weight_between_neighbours() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        assert!(l.resize(1, 20));
        let Layout::Split { weights, .. } = &l else {
            panic!()
        };
        assert_eq!(weights[0], WEIGHT_UNIT + 20, "掴んだ側が増えていない");
        assert_eq!(
            weights.iter().sum::<u16>(),
            WEIGHT_UNIT * 2,
            "総量が保存されていない（隣が道連れで縮まない）"
        );
    }

    #[test]
    fn resize_cannot_starve_a_pane() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.resize(1, 10_000);
        let Layout::Split { weights, .. } = &l else {
            panic!()
        };
        assert!(weights[1] >= 1, "隣が 0 になり消えている");
    }

    #[test]
    fn removing_a_pane_removes_its_weight() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(1, 3, Dir::Horizontal);
        l.remove(1);
        let Layout::Split { children, weights, .. } = &l else {
            panic!("2 枚残っているはず")
        };
        assert_eq!(children.len(), 2);
        assert_eq!(weights.len(), 2, "重みが取り残されて割り付けがずれる");
    }

    #[test]
    fn a_layout_without_weights_still_parses() {
        // 版 1 のスナップショットや手書き JSON を落とさない
        let json = r#"{"k":"split","dir":"horizontal","children":[{"k":"leaf","pane":1},{"k":"leaf","pane":2}]}"#;
        let l: Layout = serde_json::from_str(json).expect("重み無しが読めない");
        let Layout::Split { children, weights, .. } = &l else {
            panic!()
        };
        assert_eq!(weights_for(children, weights), vec![WEIGHT_UNIT; 2]);
    }

    #[test]
    fn messages_round_trip_as_json_lines() {
        let msg = ClientMsg::Input {
            pane: 3,
            data: encode_bytes(b"ls -la\r"),
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(!line.contains('\n'), "1行1メッセージを崩さない");
        let back: ClientMsg = serde_json::from_str(&line).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn binary_survives_base64() {
        // PTY のバイト列は UTF-8 として不正な断片を含みうる
        let raw = vec![0x1b, b'[', 0xff, 0xfe, 0x00, b'A'];
        assert_eq!(decode_bytes(&encode_bytes(&raw)).unwrap(), raw);
    }

    #[test]
    fn splitting_a_leaf_creates_a_split() {
        let mut l = Layout::leaf(1);
        assert!(l.split(1, 2, Dir::Horizontal));
        assert_eq!(l.panes(), vec![1, 2]);
    }

    #[test]
    fn same_direction_split_stays_flat() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(2, 3, Dir::Horizontal);
        // 木を深くせず兄弟として並ぶ
        match &l {
            Layout::Split { children, .. } => assert_eq!(children.len(), 3),
            _ => panic!("分割になっていない"),
        }
        assert_eq!(l.panes(), vec![1, 2, 3]);
    }

    #[test]
    fn cross_direction_split_nests() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(2, 3, Dir::Vertical);
        assert_eq!(l.panes(), vec![1, 2, 3]);
        match &l {
            Layout::Split { children, .. } => assert_eq!(children.len(), 2),
            _ => panic!("分割になっていない"),
        }
    }

    #[test]
    fn removing_collapses_single_child_splits() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        assert!(l.remove(2));
        assert_eq!(l, Layout::leaf(1), "子が1つになった分割は畳む");
    }

    #[test]
    fn removing_a_nested_leaf() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(2, 3, Dir::Vertical);
        assert!(l.remove(3));
        assert_eq!(l.panes(), vec![1, 2]);
    }
}
