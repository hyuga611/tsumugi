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

/// クライアントとサーバが話せる形の版。
///
/// **足すだけの変更では上げない。** 上げると、走っているサーバが
/// 新しい窓を受け付けなくなり、中のシェルごと止めることになる
/// （`--kill`）。足しただけなら古いサーバは新しい通を知らないだけで、
/// それは「知らない通」への答え（`ClientMsg::Unknown`）で説明できる。
///
/// **上げるのは、既にある通の意味が変わったとき**だけ — 欄の意味を変えた、
/// 単位を変えた、順序の約束を変えた。そこは黙って食い違うと画面が壊れるので、
/// 繋がせない方が安い。
pub const PROTOCOL_VERSION: u32 = 22;

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

    /// `split` の左右逆。`new_pane` を `target` の**手前**へ置く。
    ///
    /// 作業台（`go`）で木を左に出すためだけに要る。`split` してから
    /// 入れ替えると、途中の一瞬だけ木が右に出た配置がクライアントへ
    /// 配られる（画面が跳ねる）。1 通で正しい形にする。
    pub fn split_before(&mut self, target: u32, new_pane: u32, dir: Dir) -> bool {
        match self {
            Layout::Leaf { pane } if *pane == target => {
                *self = Layout::Split {
                    dir,
                    children: vec![Layout::leaf(new_pane), Layout::leaf(target)],
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
                if *my_dir == dir
                    && let Some(i) = children
                        .iter()
                        .position(|c| matches!(c, Layout::Leaf { pane } if *pane == target))
                {
                    normalize(weights, children.len());
                    let half = (weights[i] / 2).max(1);
                    weights[i] = weights[i].saturating_sub(half).max(1);
                    children.insert(i, Layout::leaf(new_pane));
                    weights.insert(i, half);
                    return true;
                }
                children
                    .iter_mut()
                    .any(|c| c.split_before(target, new_pane, dir))
            }
        }
    }

    /// その葉の取り分を決め打ちする。
    ///
    /// **兄弟の取り分は動かさない。** 比で割り付けているので、1 つだけ
    /// 大きくすれば残りは相対的に細くなる。作業台の 20 / 50 / 30 は
    /// これを 3 回呼んで作る。
    pub fn set_weight(&mut self, pane: u32, weight: u16) -> bool {
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
            weights[i] = weight.max(1);
            return true;
        }
        children.iter_mut().any(|c| c.set_weight(pane, weight))
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
    /// このペインで走っている AI エージェントが自分で名乗った状態。
    ///
    /// **推測ではなく報告。** 画面を読んで当てにいくと、エージェントが
    /// 出力の形を変えた日に黙って壊れる。名乗らないものは `None` のままにして、
    /// シェル統合（OSC 133）から分かる範囲だけをクライアント側で補う。
    #[serde(default)]
    pub agent: Option<AgentState>,
    /// **その報告に、人がもう応えたか。**
    ///
    /// 状態そのものは上書きしない（`agent` は名乗られたまま残る）。
    /// 消すのは印だけで、`--agents` や `--wait` が見るのは報告のほう。
    /// **人が応えたことは推測ではない** — そのペインへ入力が流れたのを
    /// サーバが見ている。エージェントが次に名乗ると `false` に戻る。
    #[serde(default)]
    pub agent_acked: bool,
    /// Markdown を読む形で見せているか。
    ///
    /// **サーバが持つ。** 窓を閉じて開き直したときに素のテキストへ戻ると、
    /// 読んでいた場所も見え方も失う。開いていたファイルと同じ扱いにする。
    #[serde(default)]
    pub preview: bool,
    /// このペインが木（explorer）として見せているディレクトリ。
    ///
    /// **サーバが持つ。** 開いているファイルと同じで、窓を閉じて開き直した
    /// ときに木が消えると、作業台（`go`）の左半分だけが失われる。
    #[serde(default)]
    pub dir: Option<String>,
    /// エージェントが名乗った「いくら使ったか」。表示だけに使う。
    ///
    /// **こちらでは数えない。** トークンの数え方はモデルごとに違い、
    /// 当てにいくと必ずずれる。相手が言った数字をそのまま出す。
    #[serde(default)]
    pub cost: Option<String>,
}

/// エージェントが何をしているか。`tsg --agent-state` と hooks から入る。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// 動いている
    Working,
    /// **人の返事を待っている。** 一番大事な状態。これを見つけるために作る
    Blocked,
    /// 終わった
    Done,
    /// 終わったが失敗した
    Failed,
    /// 何もしていない
    Idle,
}

impl AgentState {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "working" | "busy" | "running" => Self::Working,
            "blocked" | "waiting" | "input" => Self::Blocked,
            "done" | "finished" | "complete" => Self::Done,
            "failed" | "error" => Self::Failed,
            "idle" | "none" => Self::Idle,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Idle => "idle",
        }
    }

    /// 人が手を出す番か。ジャンプと通知はこれを見る。
    pub fn wants_you(self) -> bool {
        matches!(self, Self::Blocked | Self::Done | Self::Failed)
    }
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
    /// 人が付けた名前。無ければ中で走っているものの題名を出す。
    ///
    /// **付けた名前が勝つ。** タブに名前を付けるのは、中の題名では
    /// 区別が付かないからで、あとから題名で上書きしたら意味が無い。
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub name: String,
    pub tabs: Vec<TabInfo>,
    pub active_tab: u32,
    pub panes: Vec<PaneInfo>,
}

// ---------------------------------------------------------------------------
// 拡張（別プロセスのプラグイン）
// ---------------------------------------------------------------------------
//
// `concept.md` の「捨てるもの 5」で約束した口の続き。スクリプト言語を
// 抱えるのではなく、**外のプロセスへ意味を配り、外から語彙を足させる**。
// 中で動かさないので、拡張が落ちても本体は落ちない。

/// 拡張が受け取る出来事。
///
/// **意味の粒で配る。** 生バイト（`Output`）はもう配っているが、拡張が要るのは
/// 「コマンドが終わった」「終了コードは何だったか」であって、それを画面から
/// 当てさせると、出力の形が変わった日に黙って壊れる（左ガターと同じ
/// OSC 133 を情報源にする）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum PluginEvent {
    /// コマンドが 1 つ終わった。
    CommandEnd {
        pane: u32,
        /// シェル統合が言ってこなければ `None`。**0 で埋めない。**
        exit_code: Option<i32>,
        /// 打たれた行（取れなければ空）。
        command: String,
        /// 出力の範囲（ドキュメント絶対行）。`GetBuffer` へそのまま渡せる。
        output_start: Option<usize>,
        output_end: Option<usize>,
    },
    PaneOpened {
        pane: u32,
        cwd: Option<String>,
    },
    PaneClosed {
        pane: u32,
    },
    /// エージェントが状態を名乗った（hooks 由来。画面から当てたものではない）。
    AgentState {
        pane: u32,
        state: AgentState,
        agent: Option<String>,
    },
    /// 場所が変わった。
    Cwd {
        pane: u32,
        cwd: String,
    },
    /// 外から登録した語彙が呼ばれた。**呼んだのは人**（キー・メニュー・パレット）。
    Command {
        id: String,
        pane: Option<u32>,
        arg: Option<String>,
    },
}

impl PluginEvent {
    /// 購読の名前。`Subscribe` はこの名前で選ぶ。
    ///
    /// ペインの開閉を 1 つの名前にまとめてあるのは、片方だけ購読して
    /// 状態が片肺になる書き方を避けるため。
    pub fn name(&self) -> &'static str {
        match self {
            PluginEvent::CommandEnd { .. } => "command_end",
            PluginEvent::PaneOpened { .. } | PluginEvent::PaneClosed { .. } => "pane",
            PluginEvent::AgentState { .. } => "agent",
            PluginEvent::Cwd { .. } => "cwd",
            PluginEvent::Command { .. } => "command",
        }
    }

    /// 購読できる名前の全部。`tsg --subscribe` の案内と検証に使う。
    pub const NAMES: &'static [&'static str] = &["command_end", "pane", "agent", "cwd", "command"];
}

/// 知らせの重さ。**色を決めるためだけ**にある。
///
/// 段を増やさない。読む人が「これは 3 段目だから…」と考え始めた時点で、
/// 知らせとしては失敗している。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    #[default]
    Info,
    Warn,
    Error,
}

/// git の作業ツリー 1 つ。
///
/// **エージェントを並べるなら、置き場所も要る。** 3 本走らせるのに 3 つの
/// 枝を 1 つの作業ツリーで回すことはできない。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: String,
    /// いま出ている枝。取れなければ空（detached など）。
    pub branch: String,
    /// 本体（`git worktree list` の 1 つ目）か。
    pub main: bool,
}

/// 形だけの割り付け。**ペインの番号を持たない。**
///
/// 番号ごと書き出すと、次に当てるときには「その番号のペイン」がもう無い。
/// 持つのは形（割る向き・取り分）と、葉で何を開くか（場所と起動するもの）だけ。
///
/// これがあると、いつもの並べ方を 1 つのファイルにして配れる——
/// 「左にエディタ、右上にテスト、右下にログ」を毎朝組み直さずに済む。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum LayoutSpec {
    Leaf {
        /// そこで開く場所。書かなければ、当てるときに居た場所。
        #[serde(default)]
        cwd: Option<String>,
        /// シェルの代わりに走らせるもの。
        #[serde(default)]
        command: Option<Vec<String>>,
    },
    Split {
        dir: Dir,
        children: Vec<LayoutSpec>,
        /// 子の取り分。長さが合わなければ均等に読み替える。
        #[serde(default)]
        weights: Vec<u16>,
    },
}

impl LayoutSpec {
    /// 葉の数＝開くペインの数。
    pub fn leaves(&self) -> usize {
        match self {
            LayoutSpec::Leaf { .. } => 1,
            LayoutSpec::Split { children, .. } => children.iter().map(LayoutSpec::leaves).sum(),
        }
    }

    /// 葉を順に取り出す（開くときの順序）。
    pub fn leaf_list(&self) -> Vec<(Option<String>, Option<Vec<String>>)> {
        match self {
            LayoutSpec::Leaf { cwd, command } => vec![(cwd.clone(), command.clone())],
            LayoutSpec::Split { children, .. } => {
                children.iter().flat_map(LayoutSpec::leaf_list).collect()
            }
        }
    }

    /// 開いたペインを順に当てはめて、実際の木にする。
    ///
    /// 数が足りなければ `None`。**足りないまま組むと、葉の無い枝ができて
    /// そこへは二度と行けなくなる。**
    pub fn to_layout(&self, panes: &mut impl Iterator<Item = u32>) -> Option<Layout> {
        Some(match self {
            LayoutSpec::Leaf { .. } => Layout::leaf(panes.next()?),
            LayoutSpec::Split {
                dir,
                children,
                weights,
            } => {
                if children.is_empty() {
                    return None;
                }
                let built: Option<Vec<Layout>> =
                    children.iter().map(|c| c.to_layout(panes)).collect();
                let built = built?;
                let weights = if weights.len() == built.len() && !weights.contains(&0) {
                    weights.clone()
                } else {
                    vec![WEIGHT_UNIT; built.len()]
                };
                Layout::Split {
                    dir: *dir,
                    children: built,
                    weights,
                }
            }
        })
    }
}

/// 待つ条件。**組み合わせられる。**
///
/// 一語の決め打ち（`--wait --until done`）では、台本が書きたいことの半分も
/// 言えない。「テストが終わって、しかも終了コードが 0 でない」は 2 つの
/// 条件の重なりで、片方ずつ待つと**間の一瞬を取りこぼす**。
///
/// ⚠️ `not` は「**この一回の見比べで当たらなかった**」であって、
/// 「二度と当たらない」ではない。出力が 1 回来るたびに見比べるので、
/// `not` 単体で待つと、たいてい最初の出力で当たって返る。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "m", rename_all = "snake_case")]
pub enum Match {
    /// 画面に出た字。**打った通りに探す**（`.` も `(` もそのままの字）。
    Substring {
        text: String,
    },
    /// 正規表現。読めない式は待ち始めた時点で断る。
    Regex {
        pattern: String,
    },
    /// コマンドが終わった。`code` を書けば、その終了コードのときだけ。
    CommandEnd {
        #[serde(default)]
        code: Option<i32>,
    },
    /// エージェントがその状態を名乗った。
    Agent {
        state: AgentState,
    },
    /// その名前の出来事が起きた（`command_end` / `pane` / `agent` / `cwd` /
    /// `command`。購読と同じ名前）。
    ///
    /// **待ちと購読で別の仕組みを作らない。** 「起きるまで待つ」と
    /// 「起きたら知らせて」は同じことを別の向きから見ているだけなので、
    /// 名前の付け方まで分けると、片方だけ増える日が来る。
    Event {
        name: String,
    },
    All {
        of: Vec<Match>,
    },
    Any {
        of: Vec<Match>,
    },
    Not {
        of: Box<Match>,
    },
}

impl Match {
    /// 中の正規表現を先に組んでみる。**待ち始める前に断るため。**
    ///
    /// 待たせておいて「実は式が読めませんでした」は、待った時間が丸ごと
    /// 無駄になる（しかも黙って当たらないので原因が分からない）。
    pub fn check(&self) -> Result<(), String> {
        match self {
            Match::Regex { pattern } => regex::Regex::new(pattern)
                .map(|_| ())
                .map_err(|e| format!("正規表現を読めません: {e}")),
            Match::Event { name } => {
                if PluginEvent::NAMES.contains(&name.as_str()) {
                    Ok(())
                } else {
                    // **待たせてから「そんな出来事はありません」は最悪。**
                    Err(format!(
                        "知らない出来事です: {name}（あるのは {}）",
                        PluginEvent::NAMES.join(", ")
                    ))
                }
            }
            Match::All { of } | Match::Any { of } => of.iter().try_for_each(Match::check),
            Match::Not { of } => of.check(),
            _ => Ok(()),
        }
    }
}

/// 見比べる材料。**その一回で新しく分かったことだけ**を入れる。
pub struct MatchInput<'a> {
    /// 新しく出た字（前に見比べてから増えたぶん）。
    pub text: &'a str,
    /// このとき終わったコマンドの終了コード。終わっていなければ `None`。
    pub ended: Option<Option<i32>>,
    /// このとき名乗った状態。名乗っていなければ `None`。
    pub agent: Option<AgentState>,
    /// このとき起きた出来事の名前。
    pub event: Option<&'a str>,
}

impl Match {
    /// 当たったか。
    pub fn hit(&self, input: &MatchInput) -> bool {
        match self {
            Match::Substring { text } => input.text.contains(text.as_str()),
            Match::Regex { pattern } => regex::Regex::new(pattern)
                .map(|re| re.is_match(input.text))
                .unwrap_or(false),
            Match::CommandEnd { code } => match (input.ended, code) {
                (Some(actual), Some(want)) => actual == Some(*want),
                (Some(_), None) => true,
                (None, _) => false,
            },
            Match::Agent { state } => input.agent == Some(*state),
            Match::Event { name } => input.event == Some(name.as_str()),
            Match::All { of } => !of.is_empty() && of.iter().all(|m| m.hit(input)),
            Match::Any { of } => of.iter().any(|m| m.hit(input)),
            Match::Not { of } => !of.hit(input),
        }
    }
}

/// 拡張が何をしたかの 1 行。
///
/// **断った理由も必ず残す。** 拡張が動かないとき、人が最初に見る場所が
/// どこにも無いと、「繋がっているのに何も起きない」を調べる手がかりが
/// 画面のどこにもなくなる。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ExtLogEntry {
    /// unix 秒。**形にするのは出す側**（サーバは時計を持つだけ）。
    pub at: u64,
    /// 名乗った名前。名乗っていなければ `#3` のような接続の番号。
    pub who: String,
    pub what: String,
    /// 断った記録か。人はまずここだけを拾って読む。
    #[serde(default)]
    pub refused: bool,
}

/// 外から足された語彙。
///
/// **静的な `REGISTRY` と同じ形に落とす。** パレットも右クリックメニューも
/// キーマップもレジストリから生成しているので、同じ形にしておけば
/// 「拡張のコマンドだけ別の道」を作らずに済む。マウス経路の約束
/// （`mouse-parity.md`）も、`menu` を書かなければパレット止まりという形で
/// 拡張にそのまま効く。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ExtCommand {
    /// **`ext.` で始まること。** 名前空間を分けないと、次の版で本体が
    /// 同じ id を使った日に黙って取り合いになる。
    pub id: String,
    pub title: String,
    pub title_en: String,
    /// 既定のキー。空ならパレット（と `menu` を書けばメニュー）から。
    #[serde(default)]
    pub keys: Vec<String>,
    /// 右クリックメニューのどの節に出すか。
    #[serde(default)]
    pub menu: Option<String>,
}

impl ExtCommand {
    /// 名乗ってよい id か。
    pub fn id_is_valid(id: &str) -> bool {
        id.len() > 4
            && id.starts_with("ext.")
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Attach {
        version: u32,
        /// このクライアントが見せる大きさ。
        ///
        /// **0 は「大きさを持ち込まない」。** 台本から覗くだけのクライアント
        /// （`--send` / `--tap` / `--rpc`）が適当な 80x24 を名乗ると、
        /// その後に開くペインまでその大きさになる。窓を持たない側は 0 を送る。
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
        /// 前回の形から組み直してよいか。
        ///
        /// **決めるのはクライアント。** `cwd` は「起動した場所」が既定で
        /// 入るので、サーバから見ると指定の有無を区別できない。
        /// 「この場所を開いてくれ」と言われたのか、ただ既定が入っただけなのかは
        /// 打った側にしか分からない。
        #[serde(default)]
        restore: bool,
    },
    /// エージェントが自分の状態を名乗る（`tsg --agent-state` と hooks）。
    ///
    /// `pane` を書かなければ、そのタブでいま選ばれているペイン。
    SetAgentState {
        #[serde(default)]
        pane: Option<u32>,
        state: AgentState,
        /// 「$0.42」「12.3k tok」のような、そのまま出す文字列。
        #[serde(default)]
        cost: Option<String>,
        /// 名乗った相手（`claude` / `codex`）。
        ///
        /// **どのエージェントが居たかが分かると、再起動のあとに
        /// 「続きから」を出せる。** シェルの中で起こされた場合、ペインの
        /// プログラムはシェルなので、ここでしか分からない。
        #[serde(default)]
        agent: Option<String>,
    },
    /// 同じ入力を複数のペインへ。**エージェントを並べて同じ問いを投げる**ための口。
    Broadcast {
        /// 送り先。空なら、いまのタブで見えているペイン全部。
        #[serde(default)]
        panes: Vec<u32>,
        /// base64。
        data: String,
    },
    /// 画面の側のコマンドを外から実行する（`tsg --run <id>`）。
    ///
    /// **窓の中でしか起きないこと**（検索・ラベル・畳み・配色）を台本から
    /// 動かすための口。サーバは中身を知らず、そのまま配るだけ。
    RunCommand {
        id: String,
        /// コマンドに渡す値（検索の文字列など）。要らないものは無視する。
        #[serde(default)]
        arg: Option<String>,
    },
    /// 読む形の切り替え。`on` を書かなければ反転。
    SetPreview {
        #[serde(default)]
        pane: Option<u32>,
        #[serde(default)]
        on: Option<bool>,
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
    /// タブに名前を付ける（空なら外す）。
    RenameTab {
        tab: u32,
        name: String,
    },
    /// 定義へ移動（`gd`）。答えは `ServerMsg::Jump`。
    ///
    /// **答えが来ないことがある。** 言語サーバが入っていない・まだ
    /// 読み込み中・その位置に定義が無い。どれも普通に起きるので、
    /// 待たせない（来たら動く）。
    Definition {
        pane: u32,
        line: usize,
        col: usize,
    },
    /// 補完（入力モードで Ctrl+Space）。答えは `ServerMsg::Completions`。
    Complete {
        pane: u32,
        line: usize,
        col: usize,
    },
    /// その場で意味を訊く（`K`）。答えは `ServerMsg::Hover`。
    Hover {
        pane: u32,
        line: usize,
        col: usize,
    },
    /// 使われている場所（`gr`）。答えは `ServerMsg::Locations`。
    References {
        pane: u32,
        line: usize,
        col: usize,
    },
    /// 名前を変える（`gn`）。答えは `ServerMsg::Edits`。
    ///
    /// **当てるのはクライアント。** サーバが黙って書き換えると、
    /// 取り消し（undo）の 1 段がどこにも無いまま中身が変わる。
    Rename {
        pane: u32,
        line: usize,
        col: usize,
        new_name: String,
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
    /// クリップボードの絵を、ファイルにして置いてほしい。
    ///
    /// **書くのはサーバ。** 中を読むのはペインで走っているプログラム
    /// （AI の CLI）で、遠隔（`[domains]`）で開いていればそれは向こうに居る。
    /// こちらのディスクへ書いても、向こうからはどこにも無いパスになる。
    ///
    /// 答えは `ServerMsg::Pasted`。打ち込むのはクライアントで、
    /// **勝手に走らせない**（プロンプトへ置くところまで）。
    PasteImage {
        pane: u32,
        /// PNG（base64）。
        data: String,
    },

    // ---- 作業台（`go`） ----
    //
    // 左に木、真ん中にコード、右にエージェント。**1 通で組む。**
    // 分割・木を開く・エージェントを起こす・タブに名前を付けるを別々の通で
    // 送ると、途中の形がそのつどクライアントへ配られて画面が跳ねるうえ、
    // 「いまできたペインの ID」を追いかける非同期の穴ができる
    // （`PipeResult` を 1 通にしてあるのと同じ理由）。
    /// このペインを中央にして、作業台を組む。
    Workspace {
        pane: u32,
        /// 根。書かなければそのペインが居る場所。
        #[serde(default)]
        cwd: Option<String>,
        /// 右で起こすもの。書かなければ素のシェル。
        #[serde(default)]
        agent: Option<Vec<String>>,
    },

    // ---- 木（explorer） ----
    /// 木を並べ直す。答えは `ServerMsg::DirListing`。
    ///
    /// **開いてある枝を毎回渡す。** サーバに覚えさせると、同じセッションへ
    /// 2 枚繋いだときに片方の開き閉じがもう片方へ飛ぶ。木の見え方は
    /// フォーカスと同じでクライアントのもの。
    DirList {
        pane: u32,
        /// 根。書かなければサーバが覚えている根。
        #[serde(default)]
        root: Option<String>,
        #[serde(default)]
        expanded: Vec<String>,
    },
    /// 木を閉じる（そのペインをふつうのペインへ戻す）。
    DirClose {
        pane: u32,
    },
    /// 作る。`dir` が真ならフォルダ。**既に在れば断る**（黙って上書きしない）。
    DirNew {
        pane: u32,
        path: String,
        dir: bool,
    },
    /// 名前を変える。`to` は新しいフルパス。
    DirRename {
        pane: u32,
        from: String,
        to: String,
    },
    /// 動かす（ドラッグ＆ドロップ）。`to_dir` の中へ入れる。
    DirMove {
        pane: u32,
        from: String,
        to_dir: String,
    },

    /// 新しいタブ。`cwd` / `command` を書けばそこで開く。
    ///
    /// **tsumugi の中で `tsg` と打ったときの受け口**でもある。窓をもう 1 枚
    /// 開かずに、いまの窓のタブが増えて切り替わる。
    NewTab {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        command: Option<Vec<String>>,
    },
    SelectTab {
        tab: u32,
    },
    // ---- 拡張（別プロセス） ----
    /// 名乗る。**記録を人が読めるようにするためだけ**にある。
    ///
    /// 名乗らなくても全部動く。名乗らない拡張は記録に `#3` のような
    /// 接続の番号で出るので、どれが何をしたのか分からなくなるだけ。
    ExtHello {
        name: String,
    },
    /// 画面へ知らせる。**返事は無い。**
    ///
    /// 拡張や台本から「終わったよ」を出すための口。窓が 1 枚も開いて
    /// いなければ、どこにも出ない（溜めない）——後から出てくる知らせは、
    /// たいてい手遅れで、しかも文脈を失っている。
    Notify {
        text: String,
        #[serde(default)]
        level: Level,
    },
    /// 作業ツリーを並べる。答えは `ServerMsg::Worktrees`。
    WorktreeList {
        /// どのペインの場所で git を訊くか。書かなければ、いま選ばれているところ。
        #[serde(default)]
        pane: Option<u32>,
        /// ペインの場所が分からないときに使う場所。
        ///
        /// **シェル統合が入っていないと、ペインがどこに居るのか分からない**
        /// （OSC 7 が来ない）。打った側は自分の居場所を知っているので、
        /// それを持たせる。無ければ、出せないと答える。
        #[serde(default)]
        cwd: Option<String>,
    },
    /// 作業ツリーを足す（`git worktree add`）。
    WorktreeAdd {
        #[serde(default)]
        pane: Option<u32>,
        #[serde(default)]
        cwd: Option<String>,
        path: String,
        /// 新しく作る枝の名前。書かなければ git に任せる。
        #[serde(default)]
        branch: Option<String>,
    },
    /// 作業ツリーを消す（`git worktree remove`）。
    ///
    /// **`force` は既定で偽。** 直しかけが残っているツリーを git が断るのに
    /// 任せる。こちらで押し切ると、消えたものが戻らない。
    WorktreeRemove {
        #[serde(default)]
        pane: Option<u32>,
        #[serde(default)]
        cwd: Option<String>,
        path: String,
        #[serde(default)]
        force: bool,
    },
    /// その場所で新しいタブを開く。
    WorktreeOpen {
        path: String,
    },
    /// いまの並べ方を形だけ書き出す。答えは `ServerMsg::LayoutSpec`。
    LayoutExport {
        /// どのタブか。書かなければ、いま選ばれているもの。
        #[serde(default)]
        tab: Option<u32>,
    },
    /// 書き出した形で開く。
    ///
    /// **新しいタブに開く。** いまのタブを組み替えると、そこに居るペインを
    /// 閉じることになる。走っているものを黙って殺すのは、頼まれていない。
    LayoutApply {
        spec: LayoutSpec,
    },
    /// 条件が満たされるまで待つ。答えは `ServerMsg::Waited`（1 通だけ）。
    ///
    /// **待つのはサーバ。** 画面に出たものを全部見ているのはこちらなので、
    /// 台本が生バイトを追いかけて自分で判定する必要が無い。
    Wait {
        /// どのペインを見るか。書かなければ全部。
        #[serde(default)]
        pane: Option<u32>,
        matcher: Match,
        /// 諦めるまでの長さ。0 と書かなければ待ち続ける。
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// 拡張が何をしたかの記録。答えは `ServerMsg::ExtLog`。
    ExtLog {
        /// 新しいほうから何本まで。既定は 50。
        #[serde(default)]
        limit: Option<usize>,
    },
    /// 出来事を購読する。**名前を挙げたものだけ**が届く。
    ///
    /// 既定で全部配ると、`--tap` のつもりで繋いだ台本にまで意味の通知が
    /// 流れ込む。黙って増えない口にしておく。
    Subscribe {
        #[serde(default)]
        events: Vec<String>,
    },
    Unsubscribe {
        #[serde(default)]
        events: Vec<String>,
    },
    /// 語彙を足す。同じ id を送り直せば置き換わる。
    ///
    /// **登録した接続が切れたら消える。** 落ちた拡張のコマンドがメニューに
    /// 残り続けると、押しても何も起きない項目が増えていく。
    RegisterCommand {
        command: ExtCommand,
    },
    UnregisterCommand {
        id: String,
    },
    /// 拡張が自分のペインを開く。答えは `ServerMsg::ExtPane`。
    ///
    /// **同じ `id` で開き直すと同じペインに書く。** そうしないと、コマンドを
    /// 押すたびにペインが増えていき、片付けるのが人の仕事になる。
    ///
    /// 中身はテキスト。プロセスは持たない（`pipe_result` と同じ形）ので、
    /// スクロールも `af` も検索も、開いたファイルと同じように効く。
    ExtPaneOpen {
        id: String,
        /// どのペインの隣に開くか。書かなければ、いま選ばれているところ。
        #[serde(default)]
        near: Option<u32>,
        #[serde(default)]
        dir: Option<Dir>,
        title: String,
        text: String,
    },
    /// 開いてあるペインの中身を差し替える。無ければ何もしない。
    ExtPaneWrite {
        id: String,
        text: String,
    },
    /// 閉じる。
    ExtPaneClose {
        id: String,
    },
    /// バッファの中身を取り出す。答えは `ServerMsg::Buffer`。
    ///
    /// `--capture` が「いま見えている画面」なのに対し、こちらは
    /// **ドキュメント絶対行で範囲を指定できる**（`CommandEnd` が返す
    /// 出力の範囲をそのまま渡せる）。
    GetBuffer {
        pane: u32,
        #[serde(default)]
        start: Option<usize>,
        #[serde(default)]
        end: Option<usize>,
    },

    /// 切断するがプロセスは生かす（これが永続性の本体）。
    Detach,
    /// サーバごと落とす。
    Shutdown,
    Ping,

    /// このサーバが知らない通。
    ///
    /// **黙って捨てない。** 捨てると、新しい窓から `go` を打った人は
    /// 「何も起きない」だけを見ることになり、原因（サーバが古い）に
    /// 辿り着けない。受けた側は理由を返す。
    #[serde(other)]
    Unknown,
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
        /// primary（履歴 + primary の画面）。**alt の行は混ぜない。**
        lines: Vec<String>,
        cursor_line: usize,
        cursor_col: usize,
        /// alt screen に居るなら、その画面の行。居ないなら空。
        ///
        /// **混ぜて 1 本の文書として送ると、受けた側は alt に居ることを
        /// 知らないまま復元する。** そうなると全画面アプリの絵が履歴の
        /// 続きとして焼き付き、アプリが終わって `?1049l` が来ても
        /// 戻す先が無いので消えない（実機で踏んだ）。
        #[serde(default)]
        alt: Vec<String>,
        /// alt screen 上のカーソル（画面内の行・桁）。
        #[serde(default)]
        alt_cursor: (usize, usize),
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
    /// 画面の側で実行してほしいコマンド。
    RunCommand {
        id: String,
        #[serde(default)]
        arg: Option<String>,
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
    /// 開いているファイルの診断。**ファイルごとに総取り替え。**
    Diagnostics {
        pane: u32,
        items: Vec<tsg_lsp::Diagnostic>,
    },
    /// 定義の行き先。
    Jump {
        pane: u32,
        path: String,
        line: usize,
        col: usize,
    },
    /// 補完の候補。
    Completions {
        pane: u32,
        items: Vec<tsg_lsp::Completion>,
    },
    /// その場の意味。無ければ来ない（`error` が来る）。
    Hover {
        pane: u32,
        text: String,
    },
    /// 場所の並び（参照）。
    Locations {
        pane: u32,
        items: Vec<tsg_lsp::Location>,
    },
    /// 名前を変える書き換え。**このペインで開いているファイルのぶんだけ。**
    ///
    /// `others` は当てなかったファイルの数。0 でなければ「ここだけ変えた」と
    /// 正直に言う必要がある（黙って一部だけ当てるのが一番悪い）。
    Edits {
        pane: u32,
        edits: Vec<tsg_lsp::TextEdit>,
        others: usize,
    },
    /// ディスクの中身が変わった（作った・名前を変えた・動かした）。
    ///
    /// **中身は載せない。** どの枝を開いているかを知っているのは
    /// クライアントなので、並べ直しはクライアントが `DirList` で頼み直す。
    DirChanged {
        pane: u32,
        root: String,
    },
    /// 絵を置いた場所。クライアントはこれをプロンプトへ打ち込む。
    Pasted {
        pane: u32,
        path: String,
    },
    /// 木の中身。**総取り替え**で配る（`DirBuffer::set_entries`）。
    DirListing {
        pane: u32,
        root: String,
        rows: Vec<DirRow>,
    },
    /// 購読している出来事。
    Event {
        event: PluginEvent,
    },
    /// 外から足された語彙の全体。**総取り替えで配る。**
    ///
    /// 差分で配ると、繋いだ時点の全体像を別の通で送る必要が出て、
    /// 2 つの道が食い違う日が来る。
    ExtCommands {
        commands: Vec<ExtCommand>,
    },
    /// 画面へ出す知らせ。
    Notify {
        text: String,
        #[serde(default)]
        level: Level,
    },
    /// `worktree_list` の答え。
    Worktrees {
        items: Vec<WorktreeInfo>,
    },
    /// `layout_export` の答え。
    LayoutSpec {
        spec: LayoutSpec,
    },
    /// `wait` の答え。**1 通だけ**返る。
    Waited {
        /// 当たったか。偽なら時間切れ。
        matched: bool,
        /// 当たったペイン。
        #[serde(default)]
        pane: Option<u32>,
    },
    /// 拡張が何をしたかの記録。**古いものが先**（読む順）。
    ExtLog {
        entries: Vec<ExtLogEntry>,
    },
    /// 拡張のペインの居場所。`ext_pane_open` の返事。
    ///
    /// **番号を返す。** 拡張はこれを `get_buffer` や `input` にそのまま渡せる。
    ExtPane {
        id: String,
        pane: u32,
    },
    /// `GetBuffer` の答え。
    Buffer {
        pane: u32,
        /// `term`（端末のグリッド）か `file`（開いているファイル）。
        kind: String,
        /// `lines[0]` のドキュメント絶対行番号。
        start: usize,
        lines: Vec<String>,
    },
    Error {
        message: String,
    },

    /// この窓が知らない通。**新しいサーバに古い窓が繋いだとき。**
    ///
    /// こちらは黙って見送る — 知らせや診断の類が増えただけのことが多く、
    /// 画面を止める理由にならない。頼んだことへの答えが来ないときは、
    /// 頼んだ側が待たない作りにしてある。
    #[serde(other)]
    Unknown,
}

/// 木の 1 行。**平らにして配る。**
///
/// 入れ子のまま配ると、受け取る側がもう一度平らにすることになり、
/// 「サーバが並べた順」と「画面に出る順」が食い違う余地ができる。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DirRow {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
}

pub fn encode_bytes(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(bytes)
}

pub fn decode_bytes(s: &str) -> Option<Vec<u8>> {
    BASE64_STANDARD.decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 作業台の左は**木**。`split` は必ず後ろへ置くので、
    /// 逆向きが無いと「割ってから入れ替える」ことになり、途中の形が配られる。
    #[test]
    fn the_tree_goes_to_the_left_of_the_pane_that_asked_for_it() {
        let mut t = Layout::leaf(1);
        assert!(t.split_before(1, 2, Dir::Horizontal));
        assert_eq!(t.panes(), vec![2, 1]);
        // 同じ向きの並びの中でも手前へ入る
        assert!(t.split(1, 3, Dir::Horizontal));
        assert_eq!(t.panes(), vec![2, 1, 3]);
        assert!(t.split_before(1, 4, Dir::Horizontal));
        assert_eq!(t.panes(), vec![2, 4, 1, 3]);
    }

    /// 20 / 50 / 30。**兄弟の取り分は動かさない。**
    #[test]
    fn a_workspace_can_be_given_its_own_proportions() {
        let mut t = Layout::leaf(1);
        assert!(t.split_before(1, 2, Dir::Horizontal));
        assert!(t.split(1, 3, Dir::Horizontal));
        assert!(t.set_weight(2, 20));
        assert!(t.set_weight(1, 50));
        assert!(t.set_weight(3, 30));
        let Layout::Split {
            children, weights, ..
        } = &t
        else {
            panic!("分割になっていない");
        };
        assert_eq!(children.len(), 3);
        assert_eq!(weights, &vec![20, 50, 30]);
        assert!(!t.set_weight(99, 10), "居ないペインには効かない");
    }

    fn saw(text: &str) -> MatchInput<'_> {
        MatchInput {
            text,
            ended: None,
            agent: None,
            event: None,
        }
    }

    // ---- 形だけの割り付け -------------------------------------------------

    fn leaf() -> LayoutSpec {
        LayoutSpec::Leaf {
            cwd: None,
            command: None,
        }
    }

    #[test]
    fn a_spec_becomes_a_tree_with_the_panes_you_opened() {
        let spec = LayoutSpec::Split {
            dir: Dir::Horizontal,
            children: vec![
                leaf(),
                LayoutSpec::Split {
                    dir: Dir::Vertical,
                    children: vec![leaf(), leaf()],
                    weights: vec![30, 70],
                },
            ],
            weights: vec![60, 40],
        };
        assert_eq!(spec.leaves(), 3);

        let mut panes = [10u32, 11, 12].into_iter();
        let tree = spec.to_layout(&mut panes).expect("組めない");
        assert_eq!(tree.panes(), vec![10, 11, 12], "葉の順が入れ替わっている");
        // 取り分もそのまま写る。
        match &tree {
            Layout::Split { weights, .. } => assert_eq!(weights, &vec![60, 40]),
            Layout::Leaf { .. } => panic!("割れていない"),
        }
    }

    /// **足りないまま組まない。** 葉の無い枝ができると、そこへは二度と行けない。
    #[test]
    fn a_spec_with_too_few_panes_is_refused() {
        let spec = LayoutSpec::Split {
            dir: Dir::Horizontal,
            children: vec![leaf(), leaf()],
            weights: vec![],
        };
        let mut only_one = [10u32].into_iter();
        assert!(spec.to_layout(&mut only_one).is_none());
    }

    #[test]
    fn broken_weights_fall_back_to_equal_shares() {
        let spec = LayoutSpec::Split {
            dir: Dir::Horizontal,
            children: vec![leaf(), leaf()],
            // 長さが合わない・0 が混ざる、はどちらも均等に読み替える。
            weights: vec![0, 5, 5],
        };
        let mut panes = [1u32, 2].into_iter();
        match spec.to_layout(&mut panes).expect("組めない") {
            Layout::Split { weights, .. } => {
                assert_eq!(weights, vec![WEIGHT_UNIT, WEIGHT_UNIT]);
            }
            Layout::Leaf { .. } => panic!("割れていない"),
        }
    }

    /// 葉の場所と起動するものは、開く順に取り出せる。
    #[test]
    fn the_leaves_come_out_in_opening_order() {
        let spec = LayoutSpec::Split {
            dir: Dir::Vertical,
            children: vec![
                LayoutSpec::Leaf {
                    cwd: Some("/a".into()),
                    command: None,
                },
                LayoutSpec::Leaf {
                    cwd: Some("/b".into()),
                    command: Some(vec!["top".into()]),
                },
            ],
            weights: vec![],
        };
        let got = spec.leaf_list();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.as_deref(), Some("/a"));
        assert_eq!(
            got[1].1.as_deref().map(<[String]>::to_vec),
            Some(vec!["top".to_string()])
        );
    }

    #[test]
    fn a_substring_is_taken_as_typed() {
        // 端末で探すのはパスやエラー文。`.` も `(` もそのままの字。
        let m = Match::Substring {
            text: "a.out(1)".into(),
        };
        assert!(m.hit(&saw("running a.out(1) now")));
        assert!(
            !m.hit(&saw("running axout 1 now")),
            "正規表現として読んでいる"
        );
    }

    #[test]
    fn a_regex_is_read_as_a_pattern() {
        let m = Match::Regex {
            pattern: r"FAILED \d+".into(),
        };
        assert!(m.hit(&saw("FAILED 3 tests")));
        assert!(!m.hit(&saw("FAILED tests")));
    }

    /// **待たせてから「読めませんでした」は最悪。** 先に断れること。
    #[test]
    fn a_broken_regex_is_refused_before_waiting() {
        assert!(
            Match::Regex {
                pattern: "(".into()
            }
            .check()
            .is_err()
        );
        // 組み合わせの中に混ざっていても見つける。
        let nested = Match::All {
            of: vec![
                Match::Substring { text: "x".into() },
                Match::Not {
                    of: Box::new(Match::Regex {
                        pattern: "[".into(),
                    }),
                },
            ],
        };
        assert!(nested.check().is_err());
    }

    #[test]
    fn a_command_end_can_ask_for_a_particular_code() {
        let any = Match::CommandEnd { code: None };
        let one = Match::CommandEnd { code: Some(1) };
        let ended = |c: Option<i32>| MatchInput {
            text: "",
            ended: Some(c),
            agent: None,
            event: None,
        };
        assert!(any.hit(&ended(Some(0))));
        assert!(
            any.hit(&ended(None)),
            "終了コードが取れなくても「終わった」"
        );
        assert!(one.hit(&ended(Some(1))));
        assert!(!one.hit(&ended(Some(0))));
        // 終わっていないときは当たらない。
        assert!(!any.hit(&saw("still running")));
    }

    /// 「終わって、しかも失敗した」は 2 つの条件の重なり。
    /// **片方ずつ待つと間の一瞬を取りこぼす。**
    #[test]
    fn conditions_combine() {
        let m = Match::All {
            of: vec![
                Match::CommandEnd { code: Some(1) },
                Match::Substring {
                    text: "test".into(),
                },
            ],
        };
        let input = MatchInput {
            text: "test result: FAILED",
            ended: Some(Some(1)),
            agent: None,
            event: None,
        };
        assert!(m.hit(&input));

        let wrong_code = MatchInput {
            ended: Some(Some(0)),
            ..input
        };
        assert!(!m.hit(&wrong_code));
    }

    /// 空の `all` は当たらない。**「条件が無い」を「いつでも当たる」に
    /// しない**（書き忘れた台本が即座に返ってしまう）。
    #[test]
    fn an_empty_all_never_hits() {
        assert!(!Match::All { of: vec![] }.hit(&saw("anything")));
        assert!(!Match::Any { of: vec![] }.hit(&saw("anything")));
    }

    #[test]
    fn any_and_not_work_as_written() {
        let m = Match::Any {
            of: vec![
                Match::Substring { text: "ok".into() },
                Match::Substring {
                    text: "done".into(),
                },
            ],
        };
        assert!(m.hit(&saw("all done")));
        assert!(!m.hit(&saw("still going")));

        let n = Match::Not {
            of: Box::new(Match::Substring { text: "ok".into() }),
        };
        assert!(n.hit(&saw("failure")));
        assert!(!n.hit(&saw("ok")));
    }

    /// 待ちと購読は**同じ名前**を使う。片方だけ増える形にしない。
    #[test]
    fn an_event_can_be_waited_for_by_its_subscription_name() {
        let m = Match::Event {
            name: "pane".into(),
        };
        let happened = MatchInput {
            text: "",
            ended: None,
            agent: None,
            event: Some("pane"),
        };
        assert!(m.hit(&happened));
        assert!(!m.hit(&saw("pane")), "画面の字から当てている");
    }

    /// 知らない出来事の名前は、**待ち始める前に**断る。
    #[test]
    fn waiting_for_an_event_that_does_not_exist_is_refused_up_front() {
        assert!(
            Match::Event {
                name: "pane_opened".into()
            }
            .check()
            .is_err(),
            "購読名は `pane`。中の種類名では待てない"
        );
        assert!(
            Match::Event {
                name: "pane".into()
            }
            .check()
            .is_ok()
        );
    }

    #[test]
    fn an_agent_state_is_matched_when_it_is_announced() {
        let m = Match::Agent {
            state: AgentState::Blocked,
        };
        let named = MatchInput {
            text: "",
            ended: None,
            agent: Some(AgentState::Blocked),
            event: None,
        };
        assert!(m.hit(&named));
        assert!(!m.hit(&saw("blocked")), "画面の字から当てている");
    }

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
                } => weights.iter().all(|w| *w == WEIGHT_UNIT) && children.iter().all(all_equal),
            }
        }
        assert!(all_equal(&l), "入れ子の下まで揃っていない");
    }

    #[test]
    fn splitting_shares_only_the_split_pane_weight() {
        let mut l = Layout::leaf(1);
        l.split(1, 2, Dir::Horizontal);
        l.split(1, 3, Dir::Horizontal);
        let Layout::Split {
            weights, children, ..
        } = &l
        else {
            panic!("分割になっていない")
        };
        assert_eq!(children.len(), 3);
        assert_eq!(weights.len(), children.len(), "重みと子の数がずれている");
        // 割ったのは 1。無関係な 2 の取り分は動かない。
        assert_eq!(weights[2], WEIGHT_UNIT, "割っていないペインまで縮んでいる");
        assert_eq!(
            weights[0] + weights[1],
            WEIGHT_UNIT,
            "1 の取り分が増減している"
        );
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
        let Layout::Split {
            children, weights, ..
        } = &l
        else {
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
        let Layout::Split {
            children, weights, ..
        } = &l
        else {
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
