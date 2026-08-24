//! 走っているセッションの一覧（`Space S`）。
//!
//! ソケットを OS から列挙する手はプラットフォームごとに違い、Linux の
//! 抽象名前空間のように**そもそも列挙できない**ものもある。
//! なので当てにせず、サーバが起きたときに 1 つファイルを置き、
//! 落ちるときに消す。消し損ねた分は「繋いでみて駄目なら消す」で掃除する。
//!
//! ファイル名は安全な字だけに潰し、**本当の名前は中身に書く**。
//! 名前を潰したまま一覧に出すと、選んでも別のセッションへ繋いでしまう。

use std::path::PathBuf;

/// 置き場所。実行時の一時領域に置く（設定ではないので消えてよい）。
pub fn session_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
    };
    base.unwrap_or_else(std::env::temp_dir)
        .join("tsumugi")
        .join("sessions")
}

fn file_of(name: &str) -> PathBuf {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // 潰した結果がぶつかっても取り違えないよう、元の名前のハッシュを足す
    // （`a:b` と `a/b` はどちらも `a_b` になる）
    session_dir().join(format!("{safe}-{:08x}.session", fnv1a(name)))
}

/// 名前を短い印にするだけのハッシュ。暗号用途ではない。
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// このセッションが生きていることを書き残す。
pub fn register(name: &str) {
    let path = file_of(name);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
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
        .filter(|s| !s.is_empty())
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
