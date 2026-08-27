//! 「入れる」「外す」。**起動が面倒だと、どれだけ中身が良くても使われない。**
//!
//! 他のターミナルはインストーラが済ませていること（スタートメニュー・PATH・
//! ファイラの「ここで開く」）を、ここで 1 コマンドにまとめる。
//!
//! 方針:
//! - **exe は動かさない。** 今ある場所を指すだけにする。コピーする形にすると
//!   置き場所によっては実行が止められる環境があり（このPCがそう）、
//!   「入れたのに起動しない」という一番たちの悪い失敗になる。
//! - **何を変えたかを必ず言う。** 黙って環境を書き換えない。
//! - **`--uninstall` で全部戻る。** 戻せない変更はしない。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// 入れたもの / 外したものの記録。そのまま画面に出す。
#[derive(Default)]
pub struct Report {
    pub done: Vec<String>,
    pub notes: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Self {
            done: Vec::new(),
            notes: Vec::new(),
        }
    }
}

pub fn install() -> Result<Report> {
    let exe = std::env::current_exe().context("自分の場所が分かりません")?;
    imp::install(&exe)
}

pub fn uninstall() -> Result<Report> {
    imp::uninstall()
}

/// 最新版を取ってきて入れ替える（`tsg update`）。
///
/// **入れ方は 1 つに保つ。** ここで独自に GitHub を叩いて置き直すと、
/// README に載っている `install.ps1` と道が 2 本になり、片方だけ直した日に
/// 「入れ直したのに直らない」が起きる。だから**同じ台本を走らせる**
/// （しかも取ってくるのは常に最新の台本なので、古い exe でも新しい入れ方に従える）。
pub fn update(force: bool) -> Result<Report> {
    let exe = std::env::current_exe().context("自分の場所が分かりません")?;
    // **どの OS でも同じ判断。** ビルドの成果物へ配布版を上書きしない。
    if is_build_artifact(&exe) {
        let mut r = Report::new();
        r.notes.push(format!(
            "{} は cargo build で作ったものです。ここへ配布版を上書きしません",
            exe.display()
        ));
        r.notes
            .push("ソースを更新するなら: git pull; cargo build --release".into());
        r.notes.push(
            "配布版を別に入れるなら README の「入れる」を見てください（置き場所を選べます）".into(),
        );
        return Ok(r);
    }
    imp::update(&exe, force)
}

/// この exe は、ソースの木から `cargo build` したものか。
///
/// **そこへ配布版を上書きしない。** ビルドの成果物が黙って別物に
/// 置き換わると、次の `cargo build` まで何が動いているのか分からなくなる。
fn is_build_artifact(exe: &Path) -> bool {
    let mut parts = exe.components().rev().skip(1); // ファイル名の 1 つ上から
    let profile = parts.next();
    let target = parts.next();
    let named = |c: Option<std::path::Component<'_>>, want: &str| {
        c.and_then(|c| c.as_os_str().to_str().map(|s| s.eq_ignore_ascii_case(want)))
            .unwrap_or(false)
    };
    (named(profile, "debug") || named(profile, "release")) && named(target, "target")
}

/// アイコンを置く場所。ショートカットから参照するので、消えない場所に置く。
fn icon_path() -> Option<PathBuf> {
    crate::config::path().and_then(|p| p.parent().map(|d| d.join("tsumugi.ico")))
}

/// アイコンを書き出す。バイナリに埋め込んであるので、これ 1 つで完結する。
fn write_icon() -> Option<PathBuf> {
    let path = icon_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok()?;
    }
    std::fs::write(&path, include_bytes!("../../../assets/tsumugi.ico")).ok()?;
    Some(path)
}

// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{Report, Result, icon_path, write_icon};
    use anyhow::Context as _;
    use std::path::{Path, PathBuf};

    /// PowerShell に 1 本流す。COM（ショートカット）とレジストリのために使う。
    ///
    /// 自前で `.lnk` を書くより、OS の口を叩くほうが壊れない。
    fn ps(script: &str) -> std::io::Result<std::process::Output> {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
    }

    fn quote(p: &Path) -> String {
        p.display().to_string().replace('\'', "''")
    }

    pub fn install(exe: &Path) -> Result<Report> {
        let mut r = Report::new();
        let icon = write_icon();
        if let Some(i) = &icon {
            r.done.push(format!("アイコンを置いた: {}", i.display()));
        }
        let icon_arg = icon
            .as_ref()
            .map(|i| format!("$s.IconLocation = '{}'", quote(i)))
            .unwrap_or_default();

        // スタートメニューとデスクトップ
        for (label, folder) in [
            (
                "スタートメニュー",
                "[Environment]::GetFolderPath('Programs')",
            ),
            ("デスクトップ", "[Environment]::GetFolderPath('Desktop')"),
        ] {
            let script = format!(
                "$w = New-Object -ComObject WScript.Shell; \
                 $p = Join-Path ({folder}) 'tsumugi.lnk'; \
                 $s = $w.CreateShortcut($p); \
                 $s.TargetPath = '{exe}'; \
                 $s.WorkingDirectory = '{home}'; \
                 $s.Description = 'tsumugi terminal'; \
                 {icon_arg}; \
                 $s.Save(); $p",
                exe = quote(exe),
                home = quote(Path::new(
                    &std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into())
                )),
            );
            match ps(&script) {
                Ok(o) if o.status.success() => {
                    r.done.push(format!("{label}にショートカットを作った"));
                }
                _ => r
                    .notes
                    .push(format!("{label}のショートカットは作れなかった")),
            }
        }

        // PATH（ユーザー環境変数）。すでに入っていれば触らない。
        let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let script = format!(
            "$d = '{dir}'; \
             $p = [Environment]::GetEnvironmentVariable('Path','User'); \
             if ($p -split ';' -contains $d) {{ 'already' }} else {{ \
               [Environment]::SetEnvironmentVariable('Path', ($p.TrimEnd(';') + ';' + $d), 'User'); 'added' }}",
            dir = quote(&dir)
        );
        match ps(&script) {
            Ok(o) if o.status.success() => {
                let out = String::from_utf8_lossy(&o.stdout);
                if out.contains("added") {
                    r.done.push(format!(
                        "PATH に足した（新しいシェルから `tsg`）: {}",
                        dir.display()
                    ));
                } else {
                    r.done.push("PATH にはすでに入っていた".into());
                }
            }
            _ => r.notes.push("PATH は変えられなかった".into()),
        }

        // ファイラの「ここで開く」。HKCU だけを触る（管理者権限が要らない）。
        let icon_line = icon
            .as_ref()
            .map(|i| format!("Set-ItemProperty $k -Name Icon -Value '{}';", quote(i)))
            .unwrap_or_default();
        let script = format!(
            "$k = 'HKCU:\\Software\\Classes\\Directory\\Background\\shell\\tsumugi'; \
             New-Item -Path $k -Force | Out-Null; \
             Set-ItemProperty $k -Name '(default)' -Value 'tsumugi でここを開く'; \
             {icon_line} \
             New-Item -Path \"$k\\command\" -Force | Out-Null; \
             Set-ItemProperty \"$k\\command\" -Name '(default)' -Value '\"{exe}\" --cwd \"%V\"'",
            exe = quote(exe)
        );
        match ps(&script) {
            Ok(o) if o.status.success() => r
                .done
                .push("フォルダの右クリックに「tsumugi でここを開く」を足した".into()),
            _ => r.notes.push("右クリックメニューは足せなかった".into()),
        }

        r.notes
            .push("元に戻すときは `tsg --uninstall`。exe は動かしていない".into());
        Ok(r)
    }

    /// 入れ替える。**走っている自分は消せないので、先に名前を変える。**
    ///
    /// Windows は「走っている exe を上書き」はできないが、
    /// 「走っている exe の名前を変える」はできる。避けてから置いてもらう。
    pub fn update(exe: &Path, force: bool) -> Result<Report> {
        let mut r = Report::new();
        let dir = match std::env::var_os("TSUMUGI_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => exe
                .parent()
                .map(Path::to_path_buf)
                .context("自分の置き場所が分かりません")?,
        };

        // 前回の入れ替えで残った古い exe。**掴んでいる者が居なくなってから**
        // でないと消せないので、次の入れ替えのここで片付ける。
        sweep_old(&dir);

        // これから置かれる場所が自分自身なら、避けておく。
        let target = dir.join("tsg.exe");
        let mut moved = None;
        if same_file(exe, &target) {
            let old = dir.join(format!("tsg.exe.old-{}", std::process::id()));
            std::fs::rename(exe, &old)
                .with_context(|| format!("{} を避けられません", exe.display()))?;
            moved = Some(old);
        }

        let script = "$ErrorActionPreference='Stop'; \
             irm https://raw.githubusercontent.com/hyuga611/tsumugi/main/install.ps1 | iex";
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("TSUMUGI_DIR", &dir)
            // いまの版を教える。**同じなら 19 MB を取り直さない。**
            .env("TSUMUGI_HAVE", env!("CARGO_PKG_VERSION"));
        if force {
            cmd.env("TSUMUGI_FORCE", "1");
        }
        let status = cmd.status().context("powershell を起こせません")?;

        if !status.success() {
            // 置けなかったのに自分を避けたままだと、tsg が消えたことになる。
            if let Some(old) = &moved {
                let _ = std::fs::rename(old, exe);
            }
            anyhow::bail!("入れ替えに失敗しました（上の出力を見てください）");
        }

        // **置かれたか確かめる。** 台本は「もう最新です」と言って
        // 何もせずに戻ることがある（成功として戻る）。避けたままだと
        // tsg がどこにも無くなる — 入れ替えのつもりで消したことになる。
        if !target.exists() {
            if let Some(old) = &moved {
                std::fs::rename(old, &target)
                    .with_context(|| format!("{} を戻せません", target.display()))?;
            }
            r.notes.push(format!(
                "すでに最新です（v{}）。入れ直すなら tsg update --force",
                env!("CARGO_PKG_VERSION")
            ));
            return Ok(r);
        }

        r.done
            .push(format!("{} を最新版にしました", target.display()));
        if let Some(old) = moved {
            // **いまは消せない。** 走っているのは自分自身で、OS が実行
            // イメージを掴んでいる。抜けるのを別プロセスに見張らせる形も
            // 試したが、こちらの機械では動かなかった（見えない後始末を
            // 当てにするより、次の入れ替えで確実に消すほうを採る）。
            r.notes.push(format!(
                "古い版は {} に残ります（次の tsg update で消します）",
                old.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ));
        }
        Ok(r)
    }

    /// 同じファイルを指しているか。**字面で比べない**（`~1` の短い名前や
    /// 大文字小文字の違いで、避けるべきものを避けそこねる）。
    fn same_file(a: &Path, b: &Path) -> bool {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            // 相手がまだ無い（初めて置く）なら、名前で見るしかない。
            _ => a == b,
        }
    }

    /// 残っている古い exe を消す。**自分が付けた名前のものだけ。**
    fn sweep_old(dir: &Path) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for e in read.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("tsg.exe.old-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    pub fn uninstall() -> Result<Report> {
        let mut r = Report::new();

        for (label, folder) in [
            (
                "スタートメニュー",
                "[Environment]::GetFolderPath('Programs')",
            ),
            ("デスクトップ", "[Environment]::GetFolderPath('Desktop')"),
        ] {
            let script = format!(
                "$p = Join-Path ({folder}) 'tsumugi.lnk'; \
                 if (Test-Path $p) {{ Remove-Item $p -Force; 'removed' }}"
            );
            if let Ok(o) = ps(&script)
                && String::from_utf8_lossy(&o.stdout).contains("removed")
            {
                r.done.push(format!("{label}のショートカットを消した"));
            }
        }

        let exe = std::env::current_exe().ok();
        let dir = exe.as_deref().and_then(Path::parent).map(quote);
        if let Some(dir) = dir {
            let script = format!(
                "$d = '{dir}'; \
                 $p = [Environment]::GetEnvironmentVariable('Path','User'); \
                 $n = ($p -split ';' | Where-Object {{ $_ -ne $d -and $_ -ne '' }}) -join ';'; \
                 if ($n -ne $p) {{ [Environment]::SetEnvironmentVariable('Path', $n, 'User'); 'removed' }}"
            );
            if let Ok(o) = ps(&script)
                && String::from_utf8_lossy(&o.stdout).contains("removed")
            {
                r.done.push("PATH から外した".into());
            }
        }

        let script = "$k = 'HKCU:\\Software\\Classes\\Directory\\Background\\shell\\tsumugi'; \
             if (Test-Path $k) { Remove-Item $k -Recurse -Force; 'removed' }";
        if let Ok(o) = ps(script)
            && String::from_utf8_lossy(&o.stdout).contains("removed")
        {
            r.done.push("右クリックメニューを外した".into());
        }

        if let Some(i) = icon_path()
            && i.exists()
        {
            let _ = std::fs::remove_file(&i);
            r.done.push(format!("アイコンを消した: {}", i.display()));
        }

        r.notes
            .push("設定ファイルとセッションはそのまま。消すなら手で消す".into());
        Ok(r)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Report, Result, write_icon};
    use std::path::{Path, PathBuf};

    fn home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    pub fn install(exe: &Path) -> Result<Report> {
        let mut r = Report::new();
        if let Some(i) = write_icon() {
            r.done.push(format!("アイコンを置いた: {}", i.display()));
        }
        let Some(home) = home() else {
            r.notes.push("HOME が分からないので何もしなかった".into());
            return Ok(r);
        };

        // `~/.local/bin/tsg` から今の exe へ symlink（PATH に入っていることが多い）
        let bin = home.join(".local").join("bin");
        if std::fs::create_dir_all(&bin).is_ok() {
            let link = bin.join("tsg");
            // **他人のものを消さない。** そこに既に何かあるなら、
            // それが別の製品かもしれない。自分が張った symlink だけ張り直す。
            let ours = std::fs::symlink_metadata(&link)
                .ok()
                .is_some_and(|m| m.file_type().is_symlink());
            if link.exists() && !ours {
                r.notes.push(format!(
                    "{} に別のものが在るので触りませんでした",
                    link.display()
                ));
                return Ok(r);
            }
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            if std::os::unix::fs::symlink(exe, &link).is_ok() {
                r.done.push(format!("{} から繋いだ", link.display()));
            }
        }

        // デスクトップエントリ
        let apps = home.join(".local").join("share").join("applications");
        if std::fs::create_dir_all(&apps).is_ok() {
            let desktop = apps.join("tsumugi.desktop");
            let body = format!(
                "[Desktop Entry]\nType=Application\nName=tsumugi\n\
                 Comment=terminal you can edit\nExec={} --cwd %f\n\
                 Terminal=false\nCategories=System;TerminalEmulator;\n",
                exe.display()
            );
            if std::fs::write(&desktop, body).is_ok() {
                r.done.push(format!("{} を置いた", desktop.display()));
            }
        }
        r.notes.push("元に戻すときは `tsg --uninstall`".into());
        Ok(r)
    }

    /// 入れ替える。**Unix にはまだ配布物が無い。**
    ///
    /// Windows 向けの exe しかリリースしていないので、ここで
    /// 「入れ替えました」と言うわけにいかない。作り方を出すだけにする。
    pub fn update(_exe: &Path, _force: bool) -> Result<Report> {
        let mut r = Report::new();
        r.notes
            .push("この OS 向けの配布物はまだありません（Windows だけ）".into());
        r.notes
            .push("ソースから: git pull; cargo build --release".into());
        Ok(r)
    }

    pub fn uninstall() -> Result<Report> {
        let mut r = Report::new();
        let Some(home) = home() else {
            return Ok(r);
        };
        for p in [
            home.join(".local").join("bin").join("tsg"),
            home.join(".local")
                .join("share")
                .join("applications")
                .join("tsumugi.desktop"),
        ] {
            if p.exists() && std::fs::remove_file(&p).is_ok() {
                r.done.push(format!("{} を消した", p.display()));
            }
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::{Path, is_build_artifact};

    /// `cargo build` の成果物へ、配布版を上書きしない。
    ///
    /// **作者の機械ではこれが既定**（PATH がソースの木の `target/release` を
    /// 指している）。黙って置き換えると、次の `cargo build` まで何が
    /// 動いているのか分からなくなる。
    ///
    /// パスは**部品から組む**。`C:\...` と書くと、Unix ではそれが
    /// まるごと 1 つのファイル名になり、判定が素通りする（CI で踏んだ）。
    #[test]
    fn a_binary_built_from_source_is_left_alone() {
        let art = |parts: &[&str]| {
            let p: std::path::PathBuf = parts.iter().collect();
            is_build_artifact(&p)
        };
        assert!(art(&["dev", "tsumugi", "target", "release", "tsg.exe"]));
        assert!(art(&["home", "x", "tsumugi", "target", "debug", "tsg"]));
        // 入れた先はふつうのフォルダ。ここは入れ替えてよい。
        assert!(!art(&["Users", "x", "bin", "tsg.exe"]));
        assert!(!art(&["tools", "tsg.exe"]));
        // `target` の下でも、profile の名前でなければ違う
        assert!(!art(&["x", "target", "other", "tsg.exe"]));
        // 直下に置かれた exe（親が 1 つも無い）でも落ちない
        assert!(!is_build_artifact(Path::new("tsg.exe")));
    }
}
