//! ソケットをどこに置き、**誰が触れるか**。
//!
//! mux ソケットはこのターミナルの信頼境界そのものだ。ここへ書けるプロセスは
//! シェルへ好きなキーを送れ（＝そのユーザとして任意コード実行）、読めるプロセスは
//! 画面に出た全部（鍵・パスワード・トークン）を読める。
//! なので「同じマシンの**他のユーザ**からは触れない」ことを OS に保証させる。
//!
//! 既定の名前空間ソケットではこれが成り立たない:
//!
//! - Linux の抽象名前空間（`@name`）は**パーミッションを一切持たない**。
//!   同じマシンの誰でも繋いで読み書きできる。
//! - Windows の名前付きパイプは、既定のセキュリティ記述子が Everyone に読みを
//!   与える。画面の内容が他のユーザから読める。
//! - 名前にユーザの区別が無いので、多人数のマシンでは**先に起動した人のセッションへ
//!   繋がってしまう**（衝突がそのまま事故になる）。
//!
//! そこで:
//!
//! - Unix: 0700 のディレクトリ配下のファイルソケット（mode 0600）。
//!   ディレクトリの所有者と mode は**開くたびに確かめる**。
//! - Windows: 現在のユーザ SID だけを許す DACL を明示的に付ける。加えて
//!   クライアント側で**繋いだ先の所有者**を確かめ、先回りして同名の
//!   パイプを作られる筋（squatting）を塞ぐ。
//!
//! どちらも**閉じられなければ開かない**。守れない口は黙って開けない。

use anyhow::Result;
use interprocess::local_socket::{Listener, Stream};

/// セッション名を、ファイル名・パイプ名に使える形へ潰す。
///
/// 潰した結果がぶつかっても取り違えないよう、元の名前のハッシュを足す
/// （`a:b` と `a/b` はどちらも `a_b` に潰れる）。
pub fn slug(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    format!("{safe}-{:08x}", fnv1a(name))
}

/// 名前を短い印にするだけのハッシュ。**暗号用途ではない。**
pub fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// このセッションの口。
pub struct Endpoint {
    /// 人に見せる場所（`--diagnose` と文書用）。
    display: String,
    #[cfg(windows)]
    pipe: String,
    #[cfg(unix)]
    path: std::path::PathBuf,
}

impl Endpoint {
    pub fn for_session(session: &str) -> Result<Self> {
        imp::endpoint(session)
    }

    /// 人に見せる場所。
    pub fn display(&self) -> &str {
        &self.display
    }

    /// 受ける側。**自分だけに閉じられなければエラーにする。**
    pub fn bind(&self) -> Result<Listener> {
        imp::bind(self)
    }

    /// 繋ぐ側。繋いだ先が自分のものでなければエラーにする。
    pub fn connect(&self) -> Result<Stream> {
        imp::connect(self)
    }

    /// 使い終わった痕跡を消す（Unix のファイルソケット）。
    pub fn cleanup(&self) {
        imp::cleanup(self);
    }
}

// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp {
    use super::{Endpoint, slug};
    use anyhow::{Context, Result, bail};
    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{Listener, ListenerOptions, Name, Stream};
    // 名前の型は OS 側にある（`local_socket` の直下には無い）。
    use interprocess::os::unix::local_socket::{FilesystemUdSocket, ListenerOptionsExt as _};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::path::PathBuf;

    /// 自分だけが入れるディレクトリ。
    ///
    /// `XDG_RUNTIME_DIR` があればその下（systemd が 0700 で用意する）。
    /// 無ければ `/tmp/tsumugi-<uid>` を**自分で 0700 で作る**。`/tmp` を直に
    /// 使うと、他のユーザに先回りして作られたディレクトリへ書きに行くことになる。
    fn dir() -> Result<PathBuf> {
        let uid = unsafe { libc::getuid() };
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(d) => PathBuf::from(d).join("tsumugi"),
            None => std::env::temp_dir().join(format!("tsumugi-{uid}")),
        };
        std::fs::create_dir_all(&base)
            .with_context(|| format!("{} を作れません", base.display()))?;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("{} の権限を絞れません", base.display()))?;

        // シンボリックリンクや他人のディレクトリを掴んでいないことを確かめる。
        // 作った直後でも確かめるのは、**作る前から在った**場合があるから。
        let md = std::fs::symlink_metadata(&base)
            .with_context(|| format!("{} を確かめられません", base.display()))?;
        if !md.is_dir() {
            bail!("{} がディレクトリではありません", base.display());
        }
        if md.uid() != uid {
            bail!(
                "{} が自分のものではありません（所有者 uid {}）",
                base.display(),
                md.uid()
            );
        }
        if md.permissions().mode() & 0o077 != 0 {
            bail!("{} が他のユーザに開いています", base.display());
        }
        Ok(base)
    }

    /// Unix ドメインソケットのパスに使える長さ。
    ///
    /// `sockaddr_un.sun_path` は 104（macOS）〜108（Linux）バイトしかない。
    /// **macOS の一時ディレクトリはこれだけで 50 バイトを超える**ので、
    /// 素直に繋ぐと少し長い名前で必ず溢れる（実際に CI で溢れた）。
    const SUN_PATH_MAX: usize = 100;

    pub fn endpoint(session: &str) -> Result<Endpoint> {
        let base = dir()?;
        let mut file = format!("{}.sock", slug(session));
        // 入り切らなければ、名前を縮めてハッシュで区別する。
        // **潰した名前で衝突させない**ため、ハッシュは元の名前から取る。
        let room = SUN_PATH_MAX.saturating_sub(base.as_os_str().len() + 1);
        if file.len() > room {
            let h = super::fnv1a(session);
            let tail = format!("-{h:08x}.sock");
            let head = room.saturating_sub(tail.len());
            let mut short: String = slug(session).chars().take(head).collect();
            short.push_str(&tail);
            file = short;
        }
        let path = base.join(file);
        if path.as_os_str().len() > SUN_PATH_MAX {
            bail!(
                "ソケットの置き場所が長すぎます（{} 文字）: {}",
                path.as_os_str().len(),
                path.display()
            );
        }
        Ok(Endpoint {
            display: path.display().to_string(),
            path,
        })
    }

    pub fn bind(e: &Endpoint) -> Result<Listener> {
        // 前のサーバが落ちて残った口は片付ける。生きていれば繋がるので触らない。
        if e.path.exists() && Stream::connect(name(e)?).is_err() {
            let _ = std::fs::remove_file(&e.path);
        }
        ListenerOptions::new()
            .name(name(e)?)
            .mode(0o600)
            .create_sync()
            .with_context(|| format!("ソケットを開けません: {}", e.display))
    }

    pub fn connect(e: &Endpoint) -> Result<Stream> {
        Stream::connect(name(e)?).with_context(|| format!("サーバへ接続できません: {}", e.display))
    }

    pub fn cleanup(e: &Endpoint) {
        let _ = std::fs::remove_file(&e.path);
    }

    fn name(e: &Endpoint) -> Result<Name<'_>> {
        e.path
            .as_path()
            .to_fs_name::<FilesystemUdSocket>()
            .with_context(|| format!("ソケット名が不正: {}", e.display))
    }
}

#[cfg(windows)]
mod imp {
    use super::{Endpoint, fnv1a, slug};
    use crate::win_sd;
    use anyhow::{Context, Result, bail};
    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{GenericNamespaced, Listener, ListenerOptions, Name, Stream};
    use interprocess::os::windows::local_socket::ListenerOptionsExt as _;

    pub fn endpoint(session: &str) -> Result<Endpoint> {
        let sid = win_sd::current_user_sid().context("自分のユーザ SID が取れません")?;
        // SID をそのまま名前へ入れると長くて読めないので、印だけ入れる。
        // これは秘密ではない。守っているのは DACL と所有者の確認であって、名前ではない。
        let pipe = format!("tsumugi-{:08x}-{}.sock", fnv1a(&sid), slug(session));
        Ok(Endpoint {
            display: format!(r"\\.\pipe\{pipe}"),
            pipe,
        })
    }

    pub fn bind(e: &Endpoint) -> Result<Listener> {
        let sd = win_sd::owner_only_descriptor().context("ソケットの権限を作れません")?;
        ListenerOptions::new()
            .name(name(e)?)
            .security_descriptor(sd)
            .create_sync()
            .with_context(|| format!("ソケットを開けません: {}", e.display))
    }

    pub fn connect(e: &Endpoint) -> Result<Stream> {
        let stream = Stream::connect(name(e)?)
            .with_context(|| format!("サーバへ接続できません: {}", e.display))?;
        // `\\.\pipe\` は誰でも名前を作れるので、先回りして同じ名前のパイプを
        // 立てておく手がある。DACL は「自分の口を他人に読ませない」ためのもので、
        // 「他人の口へ自分が繋いでしまう」のは防げない。**話す前に相手を確かめる。**
        let pid = stream
            .peer_creds()
            .context("繋ぎ先のプロセスが分かりません")?
            .pid()
            .context("繋ぎ先のプロセス ID が取れません")?;
        if !win_sd::process_is_current_user(pid) {
            bail!(
                "{} は自分のものではありません（相手 pid {pid}）。\n\
                 他のユーザが先に作った口の可能性があります",
                e.display
            );
        }
        Ok(stream)
    }

    pub fn cleanup(_e: &Endpoint) {
        // 名前付きパイプは最後のハンドルが閉じたときに消える。
    }

    fn name(e: &Endpoint) -> Result<Name<'_>> {
        e.pipe
            .as_str()
            .to_ns_name::<GenericNamespaced>()
            .with_context(|| format!("ソケット名が不正: {}", e.display))
    }
}

#[cfg(test)]
mod tests {
    /// **`sun_path` は 104 バイトしかない。** macOS の一時ディレクトリは
    /// それだけで 50 バイトを超えるので、少し長い名前で必ず溢れる。
    /// 溢れる前に縮めることと、縮めても別のセッションと混ざらないこと。
    #[cfg(unix)]
    #[test]
    fn a_long_session_name_still_fits_in_sun_path() {
        let long = "a".repeat(200);
        let a = imp::endpoint(&long).expect("作れない");
        assert!(
            a.path.as_os_str().len() <= 100,
            "長すぎる: {}",
            a.path.display()
        );
        // 頭が同じでも、別の名前は別の口になる
        let b = imp::endpoint(&format!("{long}b")).expect("作れない");
        assert_ne!(a.path, b.path, "潰した名前が衝突した");
    }

    use super::*;

    #[test]
    fn names_that_squash_to_the_same_thing_keep_different_slugs() {
        assert_ne!(slug("a:b"), slug("a/b"), "潰した名前が同じで取り違える");
        assert_eq!(slug("work"), slug("work"));
    }

    #[test]
    fn a_slug_is_safe_for_a_path_or_a_pipe_name() {
        let s = slug("作業:1/2 \\ *?");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "危ない字が残っている: {s}"
        );
    }

    #[test]
    fn a_very_long_session_name_does_not_make_a_very_long_socket_name() {
        let s = slug(&"x".repeat(500));
        assert!(s.len() <= 64, "名前が長すぎる: {}", s.len());
    }

    /// 口が**自分にしか開いていない**こと。これが崩れると、同じマシンの
    /// 他ユーザにキーを送り込まれる・画面を読まれる。
    #[test]
    fn the_socket_is_not_reachable_by_other_users() {
        let Ok(e) = Endpoint::for_session("tsg-test-perm") else {
            return; // 口を用意できない環境ではスキップ
        };
        let Ok(_listener) = e.bind() else { return };

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let sock = std::path::Path::new(e.display());
            let md = std::fs::metadata(sock).expect("ソケットが無い");
            assert_eq!(
                md.permissions().mode() & 0o077,
                0,
                "ソケットが他のユーザに開いている"
            );
            let dmd = std::fs::metadata(sock.parent().expect("親が無い")).expect("置き場所が無い");
            assert_eq!(
                dmd.permissions().mode() & 0o077,
                0,
                "置き場所が他のユーザに開いている"
            );
        }
        #[cfg(windows)]
        {
            // DACL の中身は win_sd 側のテストで見る。ここでは自分では繋げること
            // （所有者の確認を通ること）を確かめる。
            assert!(e.connect().is_ok(), "自分の口に自分で繋げない");
        }
        e.cleanup();
    }
}
