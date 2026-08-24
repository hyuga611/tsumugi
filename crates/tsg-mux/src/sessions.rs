//! 走っているセッションの一覧（`Space S`）。
//!
//! ソケットを OS から列挙する手はプラットフォームごとに違い、Linux の
//! 抽象名前空間のように**そもそも列挙できない**ものもある。
//! なので当てにせず、サーバが起きたときに 1 つファイルを置き、
//! 落ちるときに消す。消し損ねた分は「繋いでみて駄目なら消す」で掃除する。
//!
//! ファイル名は安全な字だけに潰し、**本当の名前は中身に書く**。
//! 名前を潰したまま一覧に出すと、選んでも別のセッションへ繋いでしまう。
//!
//! 置き場所は**自分だけが入れるところ**に限る。ここに書ける他人が居ると、
//! 一覧に偽のセッション名を並べられる（選ぶと、その名前でサーバが起きる）。
//! `/tmp` を直に使わないのはそのため。

use std::path::PathBuf;

use crate::endpoint::slug;

/// 置き場所。実行時の一時領域に置く（設定ではないので消えてよい）。
///
/// Unix の `/tmp` は誰でも書ける。`XDG_RUNTIME_DIR` が無い環境（macOS など）
/// では uid を名前に含めた自分専用のディレクトリへ落とす。
pub fn session_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("tsumugi");
    #[cfg(unix)]
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("tsumugi"),
        // SAFETY: getuid は失敗せず、引数も取らない。
        None => std::env::temp_dir().join(format!("tsumugi-{}", unsafe { libc::getuid() })),
    };
    base.join("sessions")
}

fn file_of(name: &str) -> PathBuf {
    session_dir().join(format!("{}.session", slug(name)))
}

/// 置き場所を作り、他のユーザから閉じる。
fn ensure_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// このセッションが生きていることを書き残す。
pub fn register(name: &str) {
    let path = file_of(name);
    if let Some(dir) = path.parent()
        && ensure_dir(dir).is_err()
    {
        return;
    }
    let _ = std::fs::write(path, name);
}

pub fn unregister(name: &str) {
    let _ = std::fs::remove_file(file_of(name));
}

/// 書き残されている名前（生きているとは限らない）。
pub fn known() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(session_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "session"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .map(|s| s.trim().to_string())
        // 控えの中身はそのまま一覧に出て、選ぶとサーバが起きる。
        // 空・長すぎ・制御文字入りは受け取らない。
        .filter(|s| !s.is_empty() && s.chars().count() <= 64)
        .filter(|s| !s.chars().any(char::is_control))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// 実際に繋がるものだけを返す。**繋がらなかった控えはその場で消す。**
///
/// サーバが異常終了すると控えが残る。一覧に出したまま放っておくと、
/// 選ぶたびに新しいサーバが起きて名前だけ増えていく。
pub fn live() -> Vec<String> {
    let mut out = Vec::new();
    for name in known() {
        if crate::Client::connect(&name).is_ok() {
            out.push(name);
        } else {
            unregister(&name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_name_survives_the_round_trip() {
        // ファイル名に使えない字が入っていても、一覧には元の名前が出る。
        let name = "作業:1/2";
        register(name);
        assert!(known().contains(&name.to_string()), "潰した名前しか残っていない");
        unregister(name);
        assert!(!known().contains(&name.to_string()));
    }

    #[test]
    fn names_that_squash_to_the_same_thing_do_not_collide() {
        // どちらも `a_b` に潰れる
        register("a:b");
        register("a/b");
        let list = known();
        assert!(list.contains(&"a:b".to_string()));
        assert!(list.contains(&"a/b".to_string()));
        unregister("a:b");
        unregister("a/b");
    }
}
