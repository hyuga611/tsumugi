//! 設定ファイルの読み直し。
//!
//! **設定を直すために端末を閉じさせない。** 色や文字の大きさは「試して直す」
//! ものなので、1 回ごとに開き直させると誰も詰めない。
//!
//! 監視ライブラリは入れない。見たいのはファイル 1 つで、間隔も 1 秒でよく、
//! そのためにプラットフォーム別のイベント機構を抱えるのは釣り合わない。
//! すでに回っている 8ms のティックの上で、ときどき更新時刻を見るだけにする。

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

/// 更新時刻を見に行く間隔。
///
/// 短くしても得は無い（人が設定を保存する速さで十分）。長すぎると
/// 「保存したのに変わらない」と感じる。
const INTERVAL: Duration = Duration::from_millis(700);

pub struct Watch {
    path: Option<PathBuf>,
    next: Instant,
    /// 最後に見た更新時刻。**ファイルが無い状態も覚える**（`None`）。
    /// 覚えないと、設定を新しく作ったときに気づけない。
    stamp: Option<SystemTime>,
    seen: bool,
}

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

impl Watch {
    pub fn new() -> Self {
        Self {
            path: crate::config::path(),
            next: Instant::now() + INTERVAL,
            stamp: None,
            seen: false,
        }
    }

    /// 設定ファイルの場所。
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// 変わっていたら `true`。**呼ぶたびにファイルを見に行くわけではない。**
    pub fn changed(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next {
            return false;
        }
        self.next = now + INTERVAL;

        let Some(path) = &self.path else {
            return false;
        };
        let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let first = !self.seen;
        let changed = stamp != self.stamp;
        self.stamp = stamp;
        self.seen = true;
        // 1 回目は「変わった」とは言わない。起動直後に読み直すのは無駄。
        !first && changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch_on(path: PathBuf) -> Watch {
        Watch {
            path: Some(path),
            next: Instant::now(),
            stamp: None,
            seen: false,
        }
    }

    #[test]
    fn the_first_look_does_not_count_as_a_change() {
        let dir = std::env::temp_dir().join(format!("tsg-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("作れない");
        let f = dir.join("first.toml");
        std::fs::write(&f, "x = 1").expect("書けない");

        let mut w = watch_on(f.clone());
        assert!(!w.changed(), "起動直後に読み直そうとしている");
        let _ = std::fs::remove_file(&f);
    }

    /// 設定を**新しく作った**ときにも気づく。無い→有るは「変わった」。
    #[test]
    fn a_file_that_appears_counts_as_a_change() {
        let dir = std::env::temp_dir().join(format!("tsg-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("作れない");
        let f = dir.join("later.toml");
        let _ = std::fs::remove_file(&f);

        let mut w = watch_on(f.clone());
        w.next = Instant::now();
        assert!(!w.changed(), "1 回目は変化ではない");

        std::fs::write(&f, "x = 1").expect("書けない");
        w.next = Instant::now();
        assert!(w.changed(), "設定を作ったのに気づいていない");

        w.next = Instant::now();
        assert!(!w.changed(), "変えていないのに変わったと言っている");
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn looking_is_throttled() {
        let mut w = Watch::new();
        w.next = Instant::now() + Duration::from_secs(60);
        assert!(!w.changed(), "間隔を待たずに見に行っている");
    }
}
