//! 言語サーバの世話。**mux サーバの中で走らせる。**
//!
//! 開いているファイルを持っているのがここなので、診断もここに置く。
//! そうすると窓を閉じて開き直しても診断が消えない（開いているファイルが
//! 消えないのと同じ理由）。
//!
//! # 何本走るか
//!
//! **言語ごとに 1 本、根ごとに 1 本。** 同じ Cargo ワークスペースの
//! ファイルを 5 つ開いても rust-analyzer は 1 本。別のリポジトリを
//! 開いたらもう 1 本。
//!
//! # 入っていなかったら
//!
//! **黙って何も出さない。** 一度起こせなかった組み合わせは覚えておいて、
//! ファイルを開くたびに起こしにいかない（起こせないものを毎回試すと、
//! 開くのが目に見えて遅くなる）。

use std::collections::{BTreeMap, BTreeSet};

use tsg_lsp::{Incoming, Server, servers};

/// 待っている問い合わせ。答えが来たときに誰の何だったかを引く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    Definition { pane: u32 },
    Completion { pane: u32 },
}

/// 走っている言語サーバをまとめて持つ。
#[derive(Default)]
pub struct Lsp {
    /// (プログラム, 根) -> サーバ。
    running: BTreeMap<(String, String), Server>,
    /// 起こせなかった組み合わせ。**二度と試さない。**
    dead: BTreeSet<(String, String)>,
    /// 投げた問い合わせ。(プログラム, 根, id) -> 誰の何か。
    pending: BTreeMap<(String, String, u64), Pending>,
    /// ペイン -> そのファイルを見ているサーバの鍵。
    by_pane: BTreeMap<u32, (String, String, String)>,
    /// 設定で差し替えられた起こし方（拡張子 -> 起こし方）。
    overrides: BTreeMap<String, servers::Spec>,
}

impl Lsp {
    /// 設定から起こし方を差し替える。**既定は消さず、上に重ねる。**
    pub fn set_overrides(&mut self, specs: BTreeMap<String, servers::Spec>) {
        self.overrides = specs;
    }

    fn spec_for(&self, path: &std::path::Path) -> Option<servers::Spec> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.overrides
            .get(&ext)
            .cloned()
            .or_else(|| servers::default_for(path))
    }

    /// ファイルを開いた。要るならサーバを起こして、中身を渡す。
    ///
    /// 起こせなければ**何もしない**（診断が出ないだけ）。
    pub fn opened(&mut self, pane: u32, path: &str, text: &str) {
        let p = std::path::Path::new(path);
        let Some(spec) = self.spec_for(p) else {
            return;
        };
        let Some(root) = servers::root_for(p, &spec.roots) else {
            return;
        };
        let key = (spec.program.clone(), root.clone());
        if self.dead.contains(&key) {
            return;
        }
        if !self.running.contains_key(&key) {
            match Server::start(&spec.program, &spec.args, &root) {
                Ok(s) => {
                    self.running.insert(key.clone(), s);
                }
                Err(_) => {
                    // 入っていない。**覚えておいて二度と試さない。**
                    self.dead.insert(key);
                    return;
                }
            }
        }
        if let Some(s) = self.running.get_mut(&key) {
            let _ = s.did_open(path, &spec.language, text);
            self.by_pane
                .insert(pane, (spec.program, root, path.to_string()));
        }
    }

    /// 中身が変わった。
    pub fn changed(&mut self, pane: u32, text: &str) {
        let Some((program, root, path)) = self.by_pane.get(&pane).cloned() else {
            return;
        };
        if let Some(s) = self.running.get_mut(&(program, root)) {
            let _ = s.did_change(&path, text);
        }
    }

    /// 保存した。**型の誤りはここから出てくる**（`cargo check` が走る）。
    pub fn saved(&mut self, pane: u32) {
        let Some((program, root, path)) = self.by_pane.get(&pane).cloned() else {
            return;
        };
        if let Some(s) = self.running.get_mut(&(program, root)) {
            let _ = s.did_save(&path);
        }
    }

    /// 閉じた。
    pub fn closed(&mut self, pane: u32) {
        let Some((program, root, path)) = self.by_pane.remove(&pane) else {
            return;
        };
        if let Some(s) = self.running.get_mut(&(program, root)) {
            let _ = s.did_close(&path);
        }
    }

    /// 定義へ。投げるだけ（答えは `poll` で拾う）。
    pub fn definition(&mut self, pane: u32, line: usize, col: usize) -> bool {
        self.ask(pane, line, col, true)
    }

    /// 補完。投げるだけ。
    pub fn complete(&mut self, pane: u32, line: usize, col: usize) -> bool {
        self.ask(pane, line, col, false)
    }

    fn ask(&mut self, pane: u32, line: usize, col: usize, definition: bool) -> bool {
        let Some((program, root, path)) = self.by_pane.get(&pane).cloned() else {
            return false;
        };
        let key = (program, root);
        let Some(s) = self.running.get_mut(&key) else {
            return false;
        };
        let id = if definition {
            s.definition(&path, line, col)
        } else {
            s.completion(&path, line, col)
        };
        let Ok(id) = id else {
            return false;
        };
        let what = if definition {
            Pending::Definition { pane }
        } else {
            Pending::Completion { pane }
        };
        self.pending.insert((key.0, key.1, id), what);
        true
    }

    /// 届いているものを全部拾う。**待たない。**
    ///
    /// 返すのは (ペイン, 届いたもの, 待っていた問い合わせ)。診断は
    /// どのペインのものか分からないことがあるので、パスで引き直す。
    pub fn poll(&mut self) -> Vec<(Option<u32>, Incoming, Option<Pending>)> {
        let mut out = Vec::new();
        let keys: Vec<(String, String)> = self.running.keys().cloned().collect();
        for key in keys {
            let Some(s) = self.running.get(&key) else {
                continue;
            };
            // 溜まっているぶんだけ。**1 回の見回りで何百も抱えない**
            // （大きなリポジトリを開いた直後は一気に来る）。
            let mut n = 0;
            while n < 64 {
                let Ok(msg) = s.rx.try_recv() else { break };
                n += 1;
                match &msg {
                    Incoming::Diagnostics { path, .. } => {
                        let pane = self
                            .by_pane
                            .iter()
                            .find(|(_, (_, _, p))| same_path(p, path))
                            .map(|(id, _)| *id);
                        out.push((pane, msg, None));
                    }
                    Incoming::Answer { id, .. } => {
                        let what = self.pending.remove(&(key.0.clone(), key.1.clone(), *id));
                        let pane = what.map(|w| match w {
                            Pending::Definition { pane } | Pending::Completion { pane } => pane,
                        });
                        out.push((pane, msg, what));
                    }
                }
            }
        }
        out
    }
}

/// 同じファイルを指しているか。
///
/// **字面で比べない。** 言語サーバは `file:///c:/...` のように
/// ドライブ名を小文字で返してくることがあり（rust-analyzer で実際に
/// 踏んだ。診断が届いているのに、どのペインのものか分からず捨てていた）、
/// 区切りも `/` と `\` が混ざる。
fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.replace('\\', "/");
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 入っていない言語サーバは**一度だけ試して諦める**。
    ///
    /// 毎回試すと、開くたびにプロセスを起こそうとして目に見えて遅くなる。
    #[test]
    fn a_missing_server_is_tried_once_and_then_left_alone() {
        let mut lsp = Lsp::default();
        let mut specs = BTreeMap::new();
        specs.insert(
            "zz".to_string(),
            servers::Spec {
                language: "zz".into(),
                program: "tsumugi-no-such-language-server".into(),
                args: Vec::new(),
                roots: Vec::new(),
            },
        );
        lsp.set_overrides(specs);

        let path = std::env::temp_dir().join("a.zz");
        let path = path.display().to_string();
        lsp.opened(1, &path, "x");
        assert_eq!(lsp.dead.len(), 1, "諦めたことを覚えていない");
        assert!(lsp.running.is_empty(), "起こせていないのに走っている");

        // 2 回目は試さない（`dead` に入っているので何も起きない）。
        lsp.opened(2, &path, "x");
        assert_eq!(lsp.dead.len(), 1);
        assert!(lsp.by_pane.is_empty(), "繋がっていないのに覚えている");
    }

    /// 知らない拡張子では何も起こさない。
    #[test]
    fn an_unknown_extension_starts_nothing() {
        let mut lsp = Lsp::default();
        lsp.opened(1, "a/b.unknown-ext", "x");
        assert!(lsp.running.is_empty());
        assert!(lsp.dead.is_empty(), "起こそうともしていないのに諦めている");
    }

    /// **ドライブ名の大小と区切りの違いで取り違えない。**
    ///
    /// ここを字面で比べていたせいで、rust-analyzer からの診断が
    /// 「どのペインのものか分からない」として捨てられていた。
    #[test]
    fn the_same_file_is_recognised_however_it_is_spelled() {
        if cfg!(windows) {
            assert!(same_path(r"C:\dev\a.rs", "c:/dev/a.rs"));
            assert!(same_path(r"C:\Dev\A.rs", r"c:\dev\a.rs"));
        }
        assert!(same_path("/home/me/a.rs", "/home/me/a.rs"));
        assert!(!same_path("/home/me/a.rs", "/home/me/b.rs"));
    }

    /// 開いていないペインへの問い合わせは、投げずに断る。
    #[test]
    fn asking_about_a_pane_with_no_file_is_refused() {
        let mut lsp = Lsp::default();
        assert!(!lsp.definition(1, 0, 0));
        assert!(!lsp.complete(1, 0, 0));
    }
}
