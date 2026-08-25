//! M0-a プローブ — 設計の前提が実機で成立するかを実測する。
//!
//! `arch.md` §9 の判定ゲート。ここで落ちた前提は「後で何とかなる」種類ではないので、
//! 落ちたら設計に戻る。GPU も winit も使わない（M0-b の担当）。
//!
//! 検査するもの:
//!   1. OSC 133 が PTY を通るか（Windows では ConPTY が握り潰さないか）  ★最重要
//!   2. 実際のプロンプト統合から終了コードまで取れるか
//!   3. UTF-8 / CJK / 異体字 / 絵文字がバイト列として壊れずに通るか
//!   4. alt screen とマウスレポートを検出して所有権を裁定できるか

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use tsg_pty::{CommandBuilder, PtySession};
use tsg_term::{AmbiguousWidth, InputOwner, MouseTracking, Terminal};

const COLS: u16 = 100;
const ROWS: u16 = 30;

// ---------------------------------------------------------------------------
// シェル
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shell {
    Windows,
    Pwsh,
    Bash,
}

impl Shell {
    fn program(self) -> &'static str {
        match self {
            Shell::Windows => "powershell.exe",
            Shell::Pwsh => "pwsh",
            Shell::Bash => "bash",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Shell::Windows => "Windows PowerShell 5.1",
            Shell::Pwsh => "PowerShell 7+",
            Shell::Bash => "bash",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Shell::Windows | Shell::Pwsh => "ps1",
            Shell::Bash => "sh",
        }
    }

    fn args(self, script: &str) -> Vec<String> {
        match self {
            Shell::Windows | Shell::Pwsh => vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                script.into(),
            ],
            Shell::Bash => vec!["--norc".into(), "--noprofile".into(), script.into()],
        }
    }

    /// 実行ファイルの実体が見つかったものだけを返す。
    fn available() -> Vec<(Shell, PathBuf)> {
        let candidates = if cfg!(windows) {
            vec![Shell::Windows, Shell::Pwsh, Shell::Bash]
        } else {
            vec![Shell::Bash]
        };
        candidates
            .into_iter()
            .filter_map(|s| resolve_program(s.program()).map(|p| (s, p)))
            .collect()
    }
}

/// PATH から実行ファイルの絶対パスを引く。
///
/// Windows では `bash` が WSL のランチャ（`System32\bash.exe` /
/// `WindowsApps\bash.exe`）に先に当たることがある。これは PTY 上で
/// `execvpe(/bin/bash) failed` を吐いて即死するだけなので明示的に除外する。
/// 名前解決を OS 任せにすると、この種の事故が「ConPTY が壊れている」という
/// 誤った結論に化ける。
fn resolve_program(program: &str) -> Option<PathBuf> {
    let as_path = Path::new(program);
    if as_path.is_absolute() {
        return as_path.is_file().then(|| as_path.to_path_buf());
    }

    let exts: Vec<String> = if cfg!(windows) {
        if as_path.extension().is_some() {
            vec![String::new()]
        } else {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".EXE".into())
                .split(';')
                .map(str::to_ascii_lowercase)
                .collect()
        }
    } else {
        vec![String::new()]
    };

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if !candidate.is_file() || is_wsl_shim(&candidate) {
                continue;
            }
            return Some(candidate);
        }
    }
    None
}

fn is_wsl_shim(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    lower.ends_with("bash.exe") && (lower.contains("\\system32\\") || lower.contains("windowsapps"))
}

// ---------------------------------------------------------------------------
// 実行
// ---------------------------------------------------------------------------

/// スクリプトを PTY 上で走らせ、出力を食わせ終えた端末状態を返す。
fn run_script(shell: Shell, exe: &Path, body: &str, timeout: Duration) -> Result<Terminal> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "tsg-probe-{}-{}.{}",
        std::process::id(),
        nanos,
        shell.extension()
    ));
    std::fs::write(&path, body).context("プローブ用スクリプトの書き出しに失敗")?;

    let mut cmd = CommandBuilder::new(exe);
    for a in shell.args(&path.to_string_lossy()) {
        cmd.arg(a);
    }
    cmd.env("TERM", "xterm-256color");

    let mut session = PtySession::spawn(cmd, tsg_pty::size(COLS, ROWS))?;
    let mut reader = session.reader()?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut term = Terminal::new(COLS as usize, ROWS as usize, AmbiguousWidth::Wide);
    term.state.log_osc = true;

    let deadline = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(chunk) => term.feed(&chunk),
            Err(RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = session.kill();
    let _ = std::fs::remove_file(&path);
    Ok(term)
}

// ---------------------------------------------------------------------------
// スクリプト本体
// ---------------------------------------------------------------------------

/// 検査 1・4 用。生の OSC 133 と DEC プライベートモードを直接書き出す。
/// 日本語はコードポイントから組み立て、スクリプトファイル自体は ASCII に保つ
/// （ファイルの文字コード問題と ConPTY の問題を切り分けるため）。
fn script_direct(shell: Shell) -> String {
    match shell {
        Shell::Windows | Shell::Pwsh => r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$e = [char]27
$b = [char]7
$W = { param($s) [Console]::Out.Write($s) }

# --- 1. OSC 133 の素通し ---
& $W "$e]133;A$b"
& $W "PS> "
& $W "$e]133;B$b"
& $W "echo hi`r`n"
& $W "$e]133;C$b"
& $W "hi`r`n"
& $W "$e]133;D;0$b"

& $W "$e]133;A$b"
& $W "PS> "
& $W "$e]133;B$b"
& $W "badcmd`r`n"
& $W "$e]133;C$b"
& $W "command not found`r`n"
& $W "$e]133;D;127$b"

# --- 3. UTF-8 / CJK ---
$cjk = [char]0x65E5 + [char]0x672C + [char]0x8A9E
$amb = [char]0x203B
$ivs = [char]0x845B + [char]::ConvertFromUtf32(0xE0100)
$emo = [char]::ConvertFromUtf32(0x1F415)
& $W ("CJK|" + $cjk + "|" + $amb + "|" + $ivs + "|" + $emo + "|END`r`n")

# --- 4. alt screen + マウスレポート ---
& $W "$e[?1049h"
& $W "$e[?1002h"
& $W "$e[?1006h"
& $W "ALTSCREEN`r`n"
& $W "$e]133;A$b"
& $W "$e[?1002l"
& $W "$e[?1049l"
& $W "DONE`r`n"
"#
        .to_string(),

        Shell::Bash => r#"
printf '\033]133;A\007'
printf '$ '
printf '\033]133;B\007'
printf 'echo hi\r\n'
printf '\033]133;C\007'
printf 'hi\r\n'
printf '\033]133;D;0\007'

printf '\033]133;A\007'
printf '$ '
printf '\033]133;B\007'
printf 'badcmd\r\n'
printf '\033]133;C\007'
printf 'command not found\r\n'
printf '\033]133;D;127\007'

printf 'CJK|日本語|※|__IVS__|__EMOJI__|END\r\n'

printf '\033[?1049h'
printf '\033[?1002h'
printf '\033[?1006h'
printf 'ALTSCREEN\r\n'
printf '\033]133;A\007'
printf '\033[?1002l'
printf '\033[?1049l'
printf 'DONE\r\n'
"#
        // Git Bash の printf は C ロケールで `\U` を展開せずリテラルのまま出す。
        // 文字そのものを埋めて、バイト列が PTY を通るかだけを見る。
        .replace("__IVS__", "\u{845B}\u{E0100}")
        .replace("__EMOJI__", "\u{1F415}")
        .to_string(),
    }
}

/// 検査 2 用。実際にプロンプト関数へシェル統合を仕込み、成功・失敗コマンドを走らせる。
fn script_integration(shell: Shell) -> String {
    match shell {
        Shell::Windows | Shell::Pwsh => r#"
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$e = [char]27
$b = [char]7

function Emit-Prompt($code) {
  [Console]::Out.Write("$e]133;D;$code$b")
  [Console]::Out.Write("$e]133;A$b")
  [Console]::Out.Write("PS " + (Get-Location).Path + "> ")
  [Console]::Out.Write("$e]133;B$b")
}

Emit-Prompt 0
[Console]::Out.Write("Write-Output ok`r`n")
[Console]::Out.Write("$e]133;C$b")
Write-Output "ok"
Emit-Prompt 0

[Console]::Out.Write("cmd /c exit 3`r`n")
[Console]::Out.Write("$e]133;C$b")
cmd /c exit 3
Emit-Prompt $LASTEXITCODE

[Console]::Out.Write("exit`r`n")
"#
        .to_string(),

        Shell::Bash => r#"
emit_prompt() {
  local code=$1
  printf '\033]133;D;%s\007' "$code"
  printf '\033]133;A\007'
  printf '$ '
  printf '\033]133;B\007'
}

emit_prompt 0
printf 'echo ok\r\n'
printf '\033]133;C\007'
echo ok
emit_prompt 0

printf 'exit 3 (subshell)\r\n'
printf '\033]133;C\007'
( exit 3 )
emit_prompt $?

printf 'exit\r\n'
"#
        .to_string(),
    }
}

// ---------------------------------------------------------------------------
// 検査
// ---------------------------------------------------------------------------

struct Check {
    name: &'static str,
    passed: bool,
    detail: String,
}

fn check(name: &'static str, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        passed,
        detail: detail.into(),
    }
}

fn probe_shell(shell: Shell, exe: &Path) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    // ---- 直接書き出し ----
    let direct = run_script(shell, exe, &script_direct(shell), Duration::from_secs(20))?;
    let st = &direct.state;

    let osc133_seen = st
        .osc_log
        .iter()
        .filter(|s| s.starts_with("133") || s.starts_with("633"))
        .count();
    checks.push(check(
        "OSC 133 が PTY を通る",
        osc133_seen > 0,
        format!(
            "OSC を {} 件受信、うち 133/633 は {} 件",
            st.osc_log.len(),
            osc133_seen
        ),
    ));

    let blocks = st.marks.blocks();
    let error_block = blocks.iter().find(|b| b.is_error());
    checks.push(check(
        "コマンドブロックに畳める",
        blocks.len() >= 2,
        format!("ブロック {} 個: {:?}", blocks.len(), blocks),
    ));
    checks.push(check(
        "終了コードを取れる（]e の前提）",
        error_block.is_some_and(|b| b.exit_code == Some(127)),
        match error_block {
            Some(b) => format!("エラーブロックの終了コード = {:?}", b.exit_code),
            None => "非ゼロ終了のブロックが見つからない".to_string(),
        },
    ));

    // ---- UTF-8 / CJK ----
    let text = st.grid.document_text();
    let cjk_line = text.lines().find(|l| l.starts_with("CJK|")).unwrap_or("");
    let expects = [
        ("日本語", "\u{65E5}\u{672C}\u{8A9E}"),
        ("Ambiguous ※", "\u{203B}"),
        ("異体字 葛+IVS", "\u{845B}\u{E0100}"),
        ("絵文字 🐕", "\u{1F415}"),
    ];
    let missing: Vec<&str> = expects
        .iter()
        .filter(|(_, s)| !cjk_line.contains(s))
        .map(|(n, _)| *n)
        .collect();
    checks.push(check(
        "UTF-8 / CJK / 異体字 / 絵文字が壊れない",
        missing.is_empty() && cjk_line.ends_with("END"),
        if missing.is_empty() {
            format!("受信行: {cjk_line}")
        } else {
            format!("欠落: {missing:?} / 受信行: {cjk_line}")
        },
    ));

    // ---- alt screen と所有権 ----
    // スクリプト末尾で alt を抜けているので、履歴が汚れていないことを見る。
    let alt_leaked = text.contains("ALTSCREEN");
    checks.push(check(
        "alt screen の出力が履歴を汚さない",
        !alt_leaked,
        if alt_leaked {
            "alt screen に書いた行が primary の履歴に出ている"
        } else {
            "primary の履歴に混入なし"
        },
    ));

    // 所有権の裁定そのものは、alt に入っている最中の状態でしか見られないので
    // 別スクリプトで alt に入ったまま終わらせて確認する。
    let held = run_script(
        shell,
        exe,
        &format!(
            "{}\n{}",
            match shell {
                Shell::Bash => "printf '\\033[?1049h\\033[?1002h\\033[?1006h'",
                _ =>
                    "[Console]::Out.Write([char]27 + '[?1049h' + [char]27 + '[?1002h' + [char]27 + '[?1006h')",
            },
            match shell {
                Shell::Bash => "sleep 0.3",
                _ => "Start-Sleep -Milliseconds 300",
            }
        ),
        Duration::from_secs(15),
    )?;
    let hs = &held.state;
    let owner_ok = hs.grid.is_alt()
        && hs.modes.mouse == MouseTracking::ButtonEvent
        && hs.mouse_owner() == InputOwner::Child
        && hs.key_owner() == InputOwner::Child;
    checks.push(check(
        "alt screen + マウスレポートで所有権が子へ渡る",
        owner_ok,
        format!(
            "alt={} mouse={:?} enc={:?} -> mouse_owner={:?} key_owner={:?}",
            hs.grid.is_alt(),
            hs.modes.mouse,
            hs.modes.mouse_encoding,
            hs.mouse_owner(),
            hs.key_owner()
        ),
    ));

    // ---- 実プロンプト統合 ----
    let integ = run_script(
        shell,
        exe,
        &script_integration(shell),
        Duration::from_secs(25),
    )?;
    let ib = integ.state.marks.blocks();
    let got_three = ib.iter().any(|b| b.exit_code == Some(3));
    checks.push(check(
        "実プロンプト統合から終了コードが取れる",
        got_three,
        format!("ブロック {} 個: {:?}", ib.len(), ib),
    ));

    Ok(checks)
}

// ---------------------------------------------------------------------------

/// `tsg-probe dump <shell> <script>` — 生の受信内容を見るための切り分け用。
fn dump(shell: Shell, exe: &Path, body: &str) -> Result<()> {
    let term = run_script(shell, exe, body, Duration::from_secs(20))?;
    let st = &term.state;
    println!("--- OSC ログ ({} 件) ---", st.osc_log.len());
    for o in &st.osc_log {
        println!("  {o:?}");
    }
    println!("--- ドキュメント本文 ---");
    println!("{}", st.grid.document_text());
    println!("--- 状態 ---");
    println!(
        "alt={} mouse={:?} enc={:?} marks={} title={:?}",
        st.grid.is_alt(),
        st.modes.mouse,
        st.modes.mouse_encoding,
        st.marks.all().len(),
        st.title
    );
    Ok(())
}

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("dump") {
        let shell = match argv.get(2).map(String::as_str) {
            Some("bash") => Shell::Bash,
            Some("pwsh") => Shell::Pwsh,
            _ => Shell::Windows,
        };
        let body = match argv.get(3) {
            Some(s) => s.clone(),
            None => script_direct(shell),
        };
        let exe = resolve_program(shell.program())
            .with_context(|| format!("{} が見つかりません", shell.program()))?;
        return dump(shell, &exe, &body);
    }

    println!("tsumugi M0-a プローブ");
    println!("OS: {} / {}", std::env::consts::OS, std::env::consts::ARCH);
    println!();

    let shells = Shell::available();
    if shells.is_empty() {
        anyhow::bail!("起動できるシェルが見つかりません");
    }

    let mut all_passed = true;
    let mut critical_failed = false;

    for (shell, exe) in shells {
        println!("═══ {} ({}) ═══", shell.label(), exe.display());
        match probe_shell(shell, &exe) {
            Ok(checks) => {
                for c in &checks {
                    let mark = if c.passed { "PASS" } else { "FAIL" };
                    println!("  [{mark}] {}", c.name);
                    println!("         {}", c.detail);
                    if !c.passed {
                        all_passed = false;
                        if c.name.starts_with("OSC 133") {
                            critical_failed = true;
                        }
                    }
                }
            }
            Err(e) => {
                println!("  [SKIP] 実行できませんでした: {e:#}");
            }
        }
        println!();
    }

    println!("─────────────────────────────────────────");
    if critical_failed {
        println!("判定: 🔴 中核前提が崩れた。OSC 133 が通らない環境がある。");
        println!("      modal-spec.md のターミナル固有モーション／オブジェクトと");
        println!("      mouse-parity.md の左ガターは、この環境では成立しない。");
        println!("      -> ヒューリスティック検出への格下げを設計に入れる必要がある。");
    } else if all_passed {
        println!("判定: 🟢 M0-a の前提はすべて成立。M0-b（winit / IME / 描画）へ進んでよい。");
    } else {
        println!("判定: 🟡 中核（OSC 133）は通ったが、副次的な検査に失敗がある。");
        println!("      上の FAIL 行を確認すること。");
    }

    Ok(())
}
