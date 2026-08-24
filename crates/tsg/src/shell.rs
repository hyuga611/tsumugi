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
    fn rc_path(self) -> Option<PathBuf> {
        let home = home_dir()?;
        Some(match self {
            Shell::Bash => home.join(".bashrc"),
            Shell::Zsh => home.join(".zshrc"),
            Shell::Fish => home.join(".config").join("fish").join("config.fish"),
            Shell::Nu => home.join(".config").join("nushell").join("config.nu"),
            // PowerShell の $PROFILE は版で場所が違うので、環境から取れなければ諦める
            Shell::Pwsh => return powershell_profile(),
        })
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
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("{} を作れません", dir.display()))?;
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
    std::fs::write(&script, body)
        .with_context(|| format!("{} を書けません", script.display()))?;

    let line = shell.source_line(&script);
    let Some(rc) = shell.rc_path() else {
        bail!(
            "{} を置きました。\n\
             設定ファイルの場所が分からないので、次の 1 行を自分で足してください:\n  {line}",
            script.display()
        );
    };

    let current = std::fs::read_to_string(&rc).unwrap_or_default();
    if current.contains(shell.file_name()) {
        return Ok(format!(
            "{} を更新しました。{} はすでに読み込む設定になっています。",
            script.display(),
            rc.display()
        ));
    }

    if let Some(parent) = rc.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("\n# tsumugi shell integration\n");
    next.push_str(&line);
    next.push('\n');
    std::fs::write(&rc, next).with_context(|| format!("{} を書けません", rc.display()))?;

    Ok(format!(
        "{} 向けに {} を置き、{} に次の 1 行を足しました:\n  {line}\n\n\
         新しいシェルを開くと、プロンプトの位置と終了コードが tsumugi に伝わります。",
        shell.name(),
        script.display(),
        rc.display()
    ))
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
fn powershell_profile() -> Option<PathBuf> {
    let docs = std::env::var_os("USERPROFILE").map(PathBuf::from)?;
    for base in ["Documents", "OneDrive/Documents"] {
        for dir in ["PowerShell", "WindowsPowerShell"] {
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
        for s in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Pwsh,
            Shell::Nu,
        ] {
            let script = s.script();
            assert!(script.len() > 200, "{} のスクリプトが空", s.name());
            for mark in ["133;A", "133;B", "133;C", "133;D"] {
                assert!(
                    script.contains(mark),
                    "{} が {mark} を出していない",
                    s.name()
                );
            }
            assert!(script.contains("]7;file://"), "{} が cwd を出していない", s.name());
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
        assert!(Shell::Pwsh
            .source_line(std::path::Path::new("C:/x/tsumugi.ps1"))
            .starts_with(". "));
    }
}
