//! シェル統合（OSC 133 / OSC 7）の配布。
//!
//! **ここが入っていないと製品の中核語彙が丸ごと効かない。** `[[` `]]` `[e` `]e`、
//! 左ガターのマーカー、`ac` / `io`（コマンドブロック）は全部
//! 「プロンプトがどこか」を OSC 133 から得ている。素の cmd.exe で使うと、
//! 端末としては動くのに tsumugi である意味がほとんど無くなる。
//!
//! スクリプトはバイナリに埋め込む。別ファイルを探しに行く形にすると、
//! **exe を 1 つ置いただけの環境**（`--install-shell-integration` を最初に走らせたい環境）で
//! 何も出せなくなる。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Pwsh,
    Nu,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Self> {
        let n = name.trim().to_ascii_lowercase();
        let n = n.rsplit(['/', '\\']).next().unwrap_or(&n);
        let n = n.strip_suffix(".exe").unwrap_or(n);
        match n {
            "bash" | "sh" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "pwsh" | "powershell" | "ps1" => Some(Shell::Pwsh),
            "nu" | "nushell" => Some(Shell::Nu),
            _ => None,
        }
    }

    /// 引数が無いときの相手。`$SHELL` を見て、Windows なら PowerShell に落とす。
    pub fn detect() -> Option<Self> {
        if let Some(s) = std::env::var_os("SHELL")
            .and_then(|s| s.into_string().ok())
            .and_then(|s| Shell::parse(&s))
        {
            return Some(s);
        }
        cfg!(windows).then_some(Shell::Pwsh)
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::Pwsh => "pwsh",
            Shell::Nu => "nu",
        }
    }

    pub fn script(self) -> &'static str {
        match self {
            Shell::Bash => include_str!("../../../shell-integration/tsumugi.bash"),
            Shell::Zsh => include_str!("../../../shell-integration/tsumugi.zsh"),
            Shell::Fish => include_str!("../../../shell-integration/tsumugi.fish"),
            Shell::Pwsh => include_str!("../../../shell-integration/tsumugi.ps1"),
            Shell::Nu => include_str!("../../../shell-integration/tsumugi.nu"),
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Shell::Bash => "tsumugi.bash",
            Shell::Zsh => "tsumugi.zsh",
            Shell::Fish => "tsumugi.fish",
            Shell::Pwsh => "tsumugi.ps1",
            Shell::Nu => "tsumugi.nu",
        }
    }

    /// 置き場所を書き足す先。見つからなければ「自分で足してください」と言う。
    /// 書き足す先。**PowerShell だけは 1 つとは限らない。**
    ///
    /// Windows PowerShell 5.1 と PowerShell 7 は別の profile を読む
    /// （`WindowsPowerShell` と `PowerShell`）。両方入っている機械で
    /// 片方だけに書くと、**使っているほうでは何も起きない**（会社の PC で
    /// 実際にそうなった: 7 の profile に書いて、打っていたのは 5.1）。
    /// どちらを使うかは日によっても変わるので、**在るぶん全部**へ入れる。
    fn rc_paths(self) -> Vec<PathBuf> {
        if self == Shell::Pwsh {
            return powershell_profiles();
        }
        let Some(home) = home_dir() else {
            return Vec::new();
        };
        vec![match self {
            Shell::Bash => home.join(".bashrc"),
            Shell::Zsh => home.join(".zshrc"),
            Shell::Fish => home.join(".config").join("fish").join("config.fish"),
            Shell::Nu => home.join(".config").join("nushell").join("config.nu"),
            Shell::Pwsh => unreachable!("上で返している"),
        }]
    }

    /// rc に書き足す 1 行。
    fn source_line(self, script: &std::path::Path) -> String {
        let p = script.display();
        match self {
            Shell::Bash | Shell::Zsh => format!("[ -f \"{p}\" ] && . \"{p}\""),
            Shell::Fish => format!("test -f \"{p}\"; and source \"{p}\""),
            Shell::Nu => format!("source \"{p}\""),
            Shell::Pwsh => format!(". \"{p}\""),
        }
    }
}

/// スクリプトを置いて、rc に 1 行足す。
///
/// **すでに入っていれば何もしない。** 何度走らせても同じ状態になることを、
/// 「毎回追記して rc が太る」より優先する。
pub fn install(shell: Shell) -> Result<String> {
    let dir = script_dir().context("設定ディレクトリの場所が分かりません")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("{} を作れません", dir.display()))?;
    let script = dir.join(shell.file_name());
    // PowerShell 5.1 は BOM 無しの .ps1 を ANSI として読む。中身は ASCII だけに
    // してあるので実害は出ないが、BOM を付けておけば他の道具から見ても迷わない。
    let body = if shell == Shell::Pwsh {
        let mut b = String::from("\u{feff}");
        b.push_str(shell.script());
        b
    } else {
        shell.script().to_string()
    };
    std::fs::write(&script, body).with_context(|| format!("{} を書けません", script.display()))?;

    let line = shell.source_line(&script);
    let rcs = shell.rc_paths();
    if rcs.is_empty() {
        bail!(
            "{} を置きました。\n\
             設定ファイルの場所が分からないので、次の 1 行を自分で足してください:\n  {line}",
            script.display()
        );
    }

    let mut added: Vec<String> = Vec::new();
    let mut already: Vec<String> = Vec::new();
    for rc in &rcs {
        let current = std::fs::read_to_string(rc).unwrap_or_default();
        if current.contains(shell.file_name()) {
            already.push(rc.display().to_string());
            continue;
        }
        if let Some(parent) = rc.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut next = current;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push('\n');
        next.push_str(MARKER);
        next.push('\n');
        next.push_str(&line);
        next.push('\n');
        std::fs::write(rc, next).with_context(|| format!("{} を書けません", rc.display()))?;
        added.push(rc.display().to_string());
    }

    if added.is_empty() {
        return Ok(format!(
            "{} を更新しました。{} はすでに読み込む設定になっています。",
            script.display(),
            already.join(" / ")
        ));
    }
    let mut msg = format!(
        "{} 向けに {} を置き、次の 1 行を足しました:\n  {line}\n  足した先: {}",
        shell.name(),
        script.display(),
        added.join("\n            ")
    );
    if !already.is_empty() {
        msg.push_str(&format!("\n  すでに入っていた: {}", already.join(" / ")));
    }
    msg.push_str("\n\n新しいシェルを開くと、プロンプトの位置と終了コードが tsumugi に伝わります。");
    Ok(msg)
}

/// rc から外して、置いた台本も消す。
///
/// **入れたものは戻せること。** `--install` が人の rc を書き換える以上、
/// `--uninstall` で元へ戻らないなら、書き換えてよい理由が無い
/// （`install.rs` の「戻せない変更はしない」と同じ約束）。
///
/// 返すのは何を変えたか。何も無ければ `None`。
pub fn uninstall(shell: Shell) -> Result<Option<String>> {
    let mut changed: Vec<String> = Vec::new();

    if let Some(script) = script_dir().map(|d| d.join(shell.file_name()))
        && script.exists()
    {
        std::fs::remove_file(&script)
            .with_context(|| format!("{} を消せません", script.display()))?;
        changed.push(format!("{} を消しました", script.display()));
    }

    for rc in shell.rc_paths() {
        let Ok(current) = std::fs::read_to_string(&rc) else {
            continue;
        };
        let next = without_our_lines(&current, shell.file_name());
        if next != current {
            std::fs::write(&rc, next).with_context(|| format!("{} を書けません", rc.display()))?;
            changed.push(format!("{} から読み込む 1 行を外しました", rc.display()));
        }
    }

    Ok((!changed.is_empty()).then(|| changed.join(" / ")))
}

/// 足した行に付ける見出し。**足す側と外す側で同じものを見る。**
const MARKER: &str = "# tsumugi shell integration";

/// rc から**こちらが足した行だけ**を抜く。人が書いた行は 1 行も触らない。
///
/// 目印は 2 つ — 置いた台本の名前を含む行と、その上に付けた見出し。
/// 名前で見るので、人が場所を移していても当たる。
fn without_our_lines(current: &str, script_name: &str) -> String {
    let ends_with_newline = current.ends_with('\n');
    let mut kept: Vec<&str> = Vec::new();
    for line in current.lines() {
        if line.trim() == MARKER {
            // 見出しの前に 1 行空けて足しているので、外すときも一緒に持って帰る。
            // **元のファイルへ戻ること**が約束なので、空行 1 つでも残さない。
            if kept.last().is_some_and(|l| l.trim().is_empty()) {
                kept.pop();
            }
            continue;
        }
        if line.contains(script_name) {
            continue;
        }
        kept.push(line);
    }
    let mut next = kept.join("\n");
    if ends_with_newline && !next.is_empty() {
        next.push('\n');
    }
    next
}

fn script_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("tsumugi"))
    } else {
        home_dir().map(|h| h.join(".config").join("tsumugi"))
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// PowerShell の `$PROFILE`。版によって場所が違うので、あるものを選ぶ。
/// `$PROFILE` の場所。**当てにいかず、PowerShell 本人に訊く。**
///
/// 版（5.1 / 7）で `Documents\WindowsPowerShell` と `Documents\PowerShell` に
/// 分かれ、OneDrive を使っていれば `Documents` の場所そのものが変わる。
/// 当てにいくと 2 通りに外れる — **無いと諦める**（会社の PC でそうなった。
/// まだ profile を 1 度も作っていない人は、どちらの入れ物も無い）か、
/// **在るほうを選んでしまう**（両方あって、使っているのは別のほう）。
///
/// 訊いた答えが無ければ、これまでどおり当てにいく（PowerShell を起こせない
/// 環境でも、入るところまでは入る）。
fn powershell_profiles() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for exe in ["pwsh", "powershell"] {
        let Ok(r) = std::process::Command::new(exe)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$PROFILE.CurrentUserCurrentHost",
            ])
            .output()
        else {
            continue;
        };
        if !r.status.success() {
            continue;
        }
        let path = String::from_utf8_lossy(&r.stdout).trim().to_string();
        if path.is_empty() {
            continue;
        }
        let path = PathBuf::from(path);
        if !out.contains(&path) {
            out.push(path);
        }
    }
    if out.is_empty()
        && let Some(p) = guess_powershell_profile()
    {
        out.push(p);
    }
    out
}

/// 訊けなかったときの当て。**在る入れ物だけを見る。**
fn guess_powershell_profile() -> Option<PathBuf> {
    let docs = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
    for base in ["Documents", "OneDrive/Documents"] {
        for dir in ["WindowsPowerShell", "PowerShell"] {
            let p = docs
                .join(base.replace('/', std::path::MAIN_SEPARATOR_STR))
                .join(dir);
            if p.is_dir() {
                return Some(p.join("Microsoft.PowerShell_profile.ps1"));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::without_our_lines;

    /// 外すのは**こちらが足した行だけ**。人の rc を削らない。
    #[test]
    fn taking_our_line_back_out_leaves_everything_else_alone() {
        let rc = "Set-Alias ll Get-ChildItem\n\n# tsumugi shell integration\n. \"C:\\x\\tsumugi.ps1\"\n$env:FOO = 1\n";
        let out = without_our_lines(rc, "tsumugi.ps1");
        // 見出しの前に空けた 1 行も持って帰る（元のファイルへ戻ること）
        assert_eq!(out, "Set-Alias ll Get-ChildItem\n$env:FOO = 1\n");
    }

    /// 入っていない rc は 1 バイトも変えない（何度走らせても同じ）。
    #[test]
    fn a_file_we_never_touched_comes_back_unchanged() {
        let rc = "# mine\nSet-Alias g git\n";
        assert_eq!(without_our_lines(rc, "tsumugi.ps1"), rc);
        // 末尾の改行が無いものも、無いまま返す
        let rc2 = "# mine";
        assert_eq!(without_our_lines(rc2, "tsumugi.ps1"), rc2);
    }

    use super::*;

    #[test]
    fn shell_names_come_from_paths_too() {
        assert_eq!(Shell::parse("bash"), Some(Shell::Bash));
        assert_eq!(Shell::parse("/usr/bin/zsh"), Some(Shell::Zsh));
        assert_eq!(
            Shell::parse(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Some(Shell::Pwsh)
        );
        assert_eq!(Shell::parse("cmd.exe"), None, "cmd に統合の口は無い");
    }

    /// 埋め込みが効いていること。ファイルを探しに行く形へ戻ると、
    /// exe を 1 つ置いただけの環境で何も出せなくなる。
    #[test]
    fn every_shell_ships_a_script_that_marks_the_prompt() {
        for s in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::Pwsh, Shell::Nu] {
            let script = s.script();
            assert!(script.len() > 200, "{} のスクリプトが空", s.name());
            for mark in ["133;A", "133;B", "133;C", "133;D"] {
                assert!(
                    script.contains(mark),
                    "{} が {mark} を出していない",
                    s.name()
                );
            }
            assert!(
                script.contains("]7;file://"),
                "{} が cwd を出していない",
                s.name()
            );
        }
    }

    /// PowerShell 5.1 は BOM 無しの .ps1 を ANSI として読む。日本語コメントを入れると
    /// CP932 の 2 バイト目にバッククォートや引用符が現れて**構文が壊れる**ことがある。
    /// パイプ経由（`| Invoke-Expression`）では BOM も付けられないので、
    /// このファイルだけは ASCII に保つ。
    #[test]
    fn the_powershell_script_stays_ascii() {
        let s = Shell::Pwsh.script();
        assert!(
            s.is_ascii(),
            "tsumugi.ps1 に非 ASCII が入った。PowerShell 5.1 で読めなくなる"
        );
    }

    #[test]
    fn the_source_line_points_at_the_script() {
        let p = std::path::Path::new("/tmp/tsumugi.bash");
        assert!(Shell::Bash.source_line(p).contains("/tmp/tsumugi.bash"));
        assert!(
            Shell::Pwsh
                .source_line(std::path::Path::new("C:/x/tsumugi.ps1"))
                .starts_with(". ")
        );
    }
}
