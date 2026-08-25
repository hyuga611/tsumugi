//! 再起動をまたいでセッションを組み直す。
//!
//! ウィンドウを閉じるだけなら、mux サーバが生きているのでシェルも
//! エージェントもそのまま残る。**PC を落とすとサーバごと消える。**
//! そこを埋める。
//!
//! # 何を残して、何を残さないか
//!
//! **画面の中身は書かない。** `SECURITY.md` で「スクロールバックは
//! ディスクに残らない」と約束している。会話の本文を控えに落としたら
//! その約束が崩れる。
//!
//! 残すのは**組み立て直すための最小限**だけ:
//!
//! - タブとペインの形（分割の木）
//! - 各ペインの作業ディレクトリ
//! - 各ペインで走っていたプログラムと引数
//!
//! # 会話はどこから戻るのか
//!
//! AI エージェントは自分で会話の記録を持っている（Claude Code なら
//! `--continue`、Codex なら `resume`）。**こちらが本文を抱える必要は無い。**
//! 組み直すときに「続きから」の引数を足して起こせば、会話はそのまま続く。
//!
//! 記録の持ち主に任せるほうが、正しく続くし、こちらの約束も守れる。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::TabInfo;

/// 控えの形。**版を持つ**ので、形が変わっても古い控えで落ちない。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    pub tabs: Vec<TabInfo>,
    pub active_tab: u32,
    pub panes: Vec<SavedPane>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedPane {
    pub id: u32,
    /// 作業ディレクトリ。無ければ既定（ホーム）で開く。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 走っていたプログラムと引数。空ならシェル。
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// そのペインに居たエージェントの名前（`claude` / `codex`）。
    ///
    /// シェルの中で起こされた場合、`command` はシェルなのでここにしか残らない。
    /// **本文ではなく名前だけ。** 続きから開く言い方は相手が知っている。
    #[serde(default)]
    pub agent: Option<String>,
}

pub const VERSION: u32 = 1;

fn path_for(session: &str) -> PathBuf {
    crate::sessions::session_dir().join(format!("{}.restore", crate::endpoint::slug(session)))
}

/// 控えを置く。**置けなくても止めない**（次に開けないだけ）。
pub fn save(session: &str, saved: &Saved) {
    let path = path_for(session);
    let Ok(text) = serde_json::to_string(saved) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

/// 控えを読む。版が違うもの・壊れたものは**黙って捨てる**。
///
/// 読めない控えで起動を止めるのは本末転倒。組み直せないなら、
/// 素の 1 ペインで開けばいい。
pub fn load(session: &str) -> Option<Saved> {
    let text = std::fs::read_to_string(path_for(session)).ok()?;
    let saved: Saved = serde_json::from_str(&text).ok()?;
    (saved.version == VERSION && !saved.panes.is_empty()).then_some(saved)
}

pub fn clear(session: &str) {
    let _ = std::fs::remove_file(path_for(session));
}

/// 「続きから」で起こし直す引数。知らないプログラムはそのまま。
///
/// **こちらが会話を抱えない**ための表。記録を持っているのは相手なので、
/// 続きから開く言い方だけを知っていればいい。
pub fn resume_command(command: &[String]) -> Vec<String> {
    let Some(program) = command.first() else {
        return command.to_vec();
    };
    // `C:\...\claude.cmd` のような形でも拾えるように、名前だけを見る。
    //
    // **区切りは自分で見る。** `Path` は走っている OS の区切りしか知らないので、
    // Unix で Windows の綴りを渡すと丸ごと 1 つの名前として扱われる
    // （CI の macOS / Linux で落ちて気づいた）。控えを書いた側と読む側が
    // 同じ OS でも、試験は両方で走る。
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let name = base
        .rsplit_once('.')
        .map_or(base, |(stem, _)| stem)
        .to_ascii_lowercase();

    // 既に「続きから」が付いているなら足さない。
    let has = |flags: &[&str]| command.iter().any(|a| flags.contains(&a.as_str()));

    let mut out = command.to_vec();
    match name.as_str() {
        "claude" if !has(&["--continue", "-c", "--resume", "-r"]) => {
            out.push("--continue".into());
        }
        "codex" if !has(&["resume"]) => out.push("resume".into()),
        _ => {}
    }
    out
}

/// シェルの中で起こされたエージェントを、続きから開く 1 行。
///
/// **走らせない。** プロンプトに置くだけにして、押すかどうかは人が決める。
/// 前の続きを開くのは、たいてい望まれているが、いつもではない。
pub fn resume_line(agent: &str) -> Option<String> {
    match agent.to_ascii_lowercase().as_str() {
        "claude" => Some("claude --continue".into()),
        "codex" => Some("codex resume".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v<const N: usize>(args: [&str; N]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    /// 知っているエージェントには「続きから」を足す。
    #[test]
    fn a_known_agent_is_restarted_where_it_left_off() {
        assert_eq!(resume_command(&v(["claude"])), v(["claude", "--continue"]));
        assert_eq!(resume_command(&v(["codex"])), v(["codex", "resume"]));
        // フルパスでも名前で拾う
        assert_eq!(
            resume_command(&v([r"C:\bin\claude.cmd"])),
            v([r"C:\bin\claude.cmd", "--continue"])
        );
    }

    /// **二重に足さない。** 既に続きから開く指定があるなら、そのまま。
    #[test]
    fn an_explicit_resume_is_left_alone() {
        assert_eq!(
            resume_command(&v(["claude", "--resume", "abc"])),
            v(["claude", "--resume", "abc"])
        );
        assert_eq!(
            resume_command(&v(["codex", "resume"])),
            v(["codex", "resume"])
        );
    }

    /// 知らないものには何も足さない。**勝手な引数を渡さない。**
    #[test]
    fn anything_else_is_started_exactly_as_before() {
        assert_eq!(
            resume_command(&v(["cargo", "watch"])),
            v(["cargo", "watch"])
        );
        assert_eq!(resume_command(&[]), Vec::<String>::new());
    }

    /// シェルの中で起こされた相手にも「続きから」がある。
    #[test]
    fn an_agent_started_inside_the_shell_gets_a_line_to_press() {
        assert_eq!(resume_line("claude").as_deref(), Some("claude --continue"));
        assert_eq!(resume_line("Codex").as_deref(), Some("codex resume"));
        assert!(resume_line("vim").is_none(), "知らないものに行を作っている");
    }

    /// 版が違う控えは捨てる。**古い形で落ちない。**
    #[test]
    fn a_saved_session_from_another_version_is_ignored() {
        let dir = std::env::temp_dir().join("tsumugi-restore-test");
        let _ = std::fs::create_dir_all(&dir);
        let text = r#"{"version":999,"tabs":[],"active_tab":1,"panes":[{"id":1}]}"#;
        let saved: Option<Saved> = serde_json::from_str::<Saved>(text)
            .ok()
            .filter(|s| s.version == VERSION);
        assert!(saved.is_none());
    }
}
