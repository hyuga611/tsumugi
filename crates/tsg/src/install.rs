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

/// 配布物の名前。**リリースに載せている名前とここが唯一の対応表**
/// （`.github/workflows/release.yml`）。
fn asset_name() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", _) => "tsg.exe",
        ("macos", "aarch64") => "tsg-macos-arm64",
        ("linux", "x86_64") => "tsg-linux-x86_64",
        _ => return None,
    })
}

/// 最新版を取ってきて入れ替える（`tsg update`）。
///
/// **自分で取ってくる。外の台本を起こさない。**
///
/// 前は `powershell -Command "irm <URL> | iex"` を起こしていた。入れ方を
/// 1 通りに保つつもりだったが、**その形は「取ってきて、その場で実行する」
/// マルウェアと見分けが付かない**。実機で Windows Defender に止められ
/// （`ThreatID 2147840094`）、プロセスの起動が拒否されたうえに置いてあった
/// exe まで消された。会社の PC ほど厳しいので、まさに要る場所で壊れる。
///
/// HTTPS で取ってきてファイルに置くのは、道具が自分を更新する普通の形。
/// 最初に入れるときの `install.ps1` / `install.sh` はそのまま残す
/// （tsg がまだ無いのだから、そこは外から始めるしかない）。
pub fn update(force: bool) -> Result<Report> {
    let exe = std::env::current_exe().context("自分の場所が分かりません")?;
    let mut r = Report::new();

    // **どの OS でも同じ判断。** ビルドの成果物へ配布版を上書きしない。
    if is_build_artifact(&exe) {
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

    let Some(asset) = asset_name() else {
        r.notes.push(format!(
            "{} / {} 向けの配布物はありません",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        r.notes
            .push("ソースから: git pull; cargo build --release".into());
        return Ok(r);
    };

    let (tag, url) = latest_release(asset)?;
    if !force && tag == format!("v{}", env!("CARGO_PKG_VERSION")) {
        r.notes.push(format!(
            "すでに最新です（{tag}）。入れ直すなら tsg update --force"
        ));
        return Ok(r);
    }

    // 落としてくるのは隣へ。**途中で切れた物を tsg.exe にしない。**
    let dir = exe.parent().context("自分の置き場所が分かりません")?;
    sweep_old(dir);
    let part = dir.join("tsg.download");
    download(&url, &part).with_context(|| format!("{url} を取ってこられません"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755));
    }

    // 走っている自分は上書きできない。**名前は変えられる**ので避けてから置く
    // （Windows はファイルを追うので、開いている窓はそのまま動き続ける）。
    let old = dir.join(format!("tsg.old-{}", std::process::id()));
    let moved = std::fs::rename(&exe, &old).is_ok();
    if let Err(e) = std::fs::rename(&part, &exe) {
        let _ = std::fs::remove_file(&part);
        if moved {
            let _ = std::fs::rename(&old, &exe);
        }
        return Err(anyhow::anyhow!(e)).context(format!("{} へ置けません", exe.display()));
    }

    // 起きるところまで見る。アプリケーション制御やウイルス対策はここに出る。
    let starts = std::process::Command::new(&exe)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !starts {
        let _ = std::fs::remove_file(&exe);
        if moved {
            let _ = std::fs::rename(&old, &exe);
            r.notes
                .push("起動できなかったので、前の版へ戻しました".into());
        }
        anyhow::bail!(
            "{} を起こせません。アプリケーション制御かウイルス対策が止めている可能性があります",
            exe.display()
        );
    }

    r.done
        .push(format!("{} を {tag} にしました", exe.display()));
    if moved {
        // 自分がまだ掴んでいるので、いま消しても失敗する。次の入れ替えで消す。
        r.notes.push(format!(
            "前の版は {} に残ります（次の tsg update で消します）",
            old.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
    }
    Ok(r)
}

/// GitHub のリリースを 1 つ引いて、この機械向けの配布物の在り処を返す。
fn latest_release(asset: &str) -> Result<(String, String)> {
    let api = "https://api.github.com/repos/hyuga611/tsumugi/releases/latest";
    let body = ureq::get(api)
        .header("User-Agent", "tsumugi-update")
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("GitHub に繋がりません")?
        .body_mut()
        .read_to_string()
        .context("リリースの一覧を読めません")?;
    let v: serde_json::Value = serde_json::from_str(&body).context("リリースの形が読めません")?;
    let tag = v["tag_name"]
        .as_str()
        .context("リリースに版の名前がありません")?
        .to_string();
    let url = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(asset))
        .and_then(|a| a["browser_download_url"].as_str())
        .with_context(|| format!("{tag} に {asset} がありません"))?
        .to_string();
    Ok((tag, url))
}

/// 取ってきてファイルへ。**丸ごと覚えない**（20 MB を抱えない）。
fn download(url: &str, to: &std::path::Path) -> Result<()> {
    let mut res = ureq::get(url)
        .header("User-Agent", "tsumugi-update")
        .call()?;
    let mut file = std::fs::File::create(to)?;
    std::io::copy(&mut res.body_mut().as_reader(), &mut file)?;
    Ok(())
}

/// 残っている古い exe を消す。**自分が付けた名前のものだけ。**
fn sweep_old(dir: &std::path::Path) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for e in read.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("tsg.old-") || name.starts_with("tsg.exe.old-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
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
    use std::path::Path;

    /// PowerShell に 1 本流す。COM（ショートカット）とレジストリのために使う。
    ///
    /// 自前で `.lnk` を書くより、OS の口を叩くほうが壊れない。
    fn ps(script: &str) -> std::io::Result<std::process::Output> {
        // 名前だけで引くと、PATH の具合や仕組みの都合で起こせないことがある
        // （会社の PC で踏んだ）。絶対パスから順に試す。
        let exe = working_powershell().unwrap_or_else(|_| std::path::PathBuf::from("powershell"));
        std::process::Command::new(exe)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .output()
    }

    /// 起こせる PowerShell を探す。**名前だけに頼らない。**
    ///
    /// 会社の PC で `powershell` を起こそうとして
    /// 「アクセスが拒否されました（os error 5）」が出た。PATH の解決か、
    /// 落としてきた exe から子を起こすことを止める仕組み（アプリケーション
    /// 制御・ウイルス対策）か、こちらからは切り分けられない。**絶対パスと
    /// 別の実装を順に試し、それでも駄目なら何を試したかを言う。**
    fn powershell_candidates() -> Vec<std::path::PathBuf> {
        let mut out: Vec<std::path::PathBuf> = Vec::new();
        if let Some(root) = std::env::var_os("SystemRoot") {
            let root = std::path::PathBuf::from(root);
            out.push(root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
            out.push(root.join(r"SysWOW64\WindowsPowerShell\v1.0\powershell.exe"));
        }
        out.push(std::path::PathBuf::from("powershell"));
        out.push(std::path::PathBuf::from("pwsh"));
        out
    }

    /// 実際に起こせるものを 1 つ返す。**何も起こせなければ、試した先を添えて返す。**
    ///
    /// 避ける前にここを通す。起こせないと分かってから名前を戻すのでは、
    /// 「戻し損ねたら tsg が消える」道を毎回通ることになる。
    fn working_powershell() -> Result<std::path::PathBuf, String> {
        let mut tried: Vec<String> = Vec::new();
        for exe in powershell_candidates() {
            match std::process::Command::new(&exe)
                .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
                .output()
            {
                Ok(o) if o.status.success() => return Ok(exe),
                Ok(o) => tried.push(format!("{} -> 終了コード {}", exe.display(), o.status)),
                Err(e) => tried.push(format!("{} -> {e}", exe.display())),
            }
        }
        Err(tried.join("\n    "))
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
