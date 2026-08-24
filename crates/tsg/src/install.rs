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

use std::path::PathBuf;

use anyhow::{Context, Result};

/// 入れたもの / 外したものの記録。そのまま画面に出す。
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
    use std::path::Path;

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
            ("スタートメニュー", "[Environment]::GetFolderPath('Programs')"),
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
                _ => r.notes.push(format!("{label}のショートカットは作れなかった")),
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
                    r.done
                        .push(format!("PATH に足した（新しいシェルから `tsg`）: {}", dir.display()));
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

    pub fn uninstall() -> Result<Report> {
        let mut r = Report::new();

        for (label, folder) in [
            ("スタートメニュー", "[Environment]::GetFolderPath('Programs')"),
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
    /// アイコンは埋め込む。別ファイルを探しに行く形にすると、
    /// exe を 1 つ置いただけの環境で「入れる」が成立しない。
    #[test]
    fn the_icon_is_embedded_in_the_binary() {
        let ico = include_bytes!("../../../assets/tsumugi.ico");
        assert!(ico.len() > 1000, "アイコンが空");
        assert_eq!(&ico[0..4], &[0, 0, 1, 0], "ICO の形をしていない");
    }

    #[test]
    fn the_window_icon_is_a_square_of_rgba() {
        let rgba = include_bytes!("../../../assets/icon.rgba");
        assert_eq!(rgba.len(), 256 * 256 * 4, "256x256 の RGBA ではない");
    }
}
