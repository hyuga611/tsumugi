//! どの言語にどのサーバを起こすか。
//!
//! **入っていなければ何も起きない**のが既定。ここに並んでいるのは
//! 「入っていたら使う」という表で、入れさせるための表ではない。
//!
//! 設定で足せる・差し替えられる（`[lsp.<言語>]`）。既定を消さずに
//! 上へ重ねる形にしてあるので、書かなかった言語は今までどおり。

use std::path::Path;

/// 1 つの言語サーバの起こし方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// LSP に名乗る言語名（`rust` `typescript` など）。
    pub language: String,
    /// 起こすプログラム。
    pub program: String,
    pub args: Vec<String>,
    /// これが在るディレクトリを根と見なす（`Cargo.toml` など）。
    ///
    /// **根を間違えると診断が出ない。** rust-analyzer は
    /// `Cargo.toml` の在るところを渡さないと、何も返さないことがある。
    pub roots: Vec<String>,
}

fn spec(language: &str, program: &str, args: &[&str], roots: &[&str]) -> Spec {
    Spec {
        language: language.into(),
        program: program.into(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        roots: roots.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// 拡張子から既定のサーバを引く。知らない拡張子は `None`。
///
/// **広げすぎない。** ここに並べるのは、入っている率が高くて
/// 起こし方が 1 つに決まるものだけ。迷うものは設定に書いてもらう。
pub fn default_for(path: &Path) -> Option<Spec> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => spec("rust", "rust-analyzer", &[], &["Cargo.toml"]),
        "go" => spec("go", "gopls", &[], &["go.mod"]),
        "py" => spec(
            "python",
            "pyright-langserver",
            &["--stdio"],
            &["pyproject.toml", "setup.py", "requirements.txt"],
        ),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => spec(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            &["package.json", "tsconfig.json"],
        ),
        "c" | "h" | "cc" | "cpp" | "hpp" => spec("cpp", "clangd", &[], &["compile_commands.json"]),
        _ => return None,
    })
}

/// そのファイルの「根」。目印のファイルを上へ探す。
///
/// 見つからなければファイルの在るディレクトリ。**探しに行かないより、
/// 近いところで起こすほうがまし**（何も出ないよりは出る）。
pub fn root_for(path: &Path, marks: &[String]) -> Option<String> {
    let dir = path.parent()?;
    let mut at = Some(dir);
    while let Some(d) = at {
        if marks.iter().any(|m| d.join(m).exists()) {
            return Some(d.display().to_string());
        }
        at = d.parent();
    }
    Some(dir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_picks_the_server() {
        let s = default_for(Path::new("a/b.rs")).expect("rust が引けない");
        assert_eq!(s.program, "rust-analyzer");
        assert_eq!(s.language, "rust");

        let s = default_for(Path::new("a/b.tsx")).expect("ts が引けない");
        assert_eq!(s.program, "typescript-language-server");

        assert!(
            default_for(Path::new("a/b.txt")).is_none(),
            "知らない拡張子に何か返している"
        );
        assert!(default_for(Path::new("noext")).is_none());
    }

    /// 目印のあるところまで上がる。
    #[test]
    fn the_root_is_where_the_marker_lives() {
        let base = std::env::temp_dir().join(format!("tsumugi-root-{}", std::process::id()));
        let deep = base.join("crates").join("x").join("src");
        std::fs::create_dir_all(&deep).expect("作れない");
        std::fs::write(base.join("Cargo.toml"), "").expect("書けない");

        let got = root_for(&deep.join("lib.rs"), &["Cargo.toml".to_string()]);
        assert_eq!(got.as_deref(), Some(base.display().to_string().as_str()));

        // 目印が無ければ、そのファイルの在るところ。
        let got = root_for(&deep.join("lib.rs"), &["nothing-here".to_string()]);
        assert_eq!(got.as_deref(), Some(deep.display().to_string().as_str()));

        let _ = std::fs::remove_dir_all(&base);
    }
}
