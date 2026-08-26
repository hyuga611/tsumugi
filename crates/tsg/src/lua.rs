//! 設定を **計算して** 書く道（`config.lua`）。
//!
//! `concept.md` の「捨てるもの 5」は、v1 に組み込みスクリプト言語を入れない
//! 代わりに RPC の口を開ける、と決めた。その口（`rpc.md` §5）は**別プロセス**の
//! ためのもので、そこでは届かないものが 2 つだけ残る。
//!
//! 1. **起動より前に決まっていてほしいもの**（字体・不透明度・配色）。
//!    窓が開いた後に外から直すと、開いた瞬間だけ違う見た目が出る。
//! 2. **機械ごとに違うもの**。同じ設定ファイルを 2 台で使うと、宣言だけの
//!    TOML では「この機械のときはこう」が書けない。
//!
//! だから Lua が担うのは**設定を計算すること**だけで、拡張の場ではない。
//! 重いものは口の向こう（別プロセス）へ置く。中で走らせると、拡張が落ちた
//! ときに端末ごと落ちる。
//!
//! ## 形は TOML と同じ
//!
//! 返すのは `config.toml` と**同じ形の表**。読み取りと検証は今までの道を
//! そのまま通す（`config.rs`）。Lua 用に別の検証を書くと、片方だけ直った
//! 設定項目が必ず出る。
//!
//! ```lua
//! local t = { window = { opacity = 0.85 } }
//! if tsumugi.hostname == "work" then
//!   t.theme = { name = "sumi" }
//! end
//! return t
//! ```

use std::path::{Path, PathBuf};

use mlua::{Lua, Value};

/// `config.lua` の置き場所。`config.toml` の隣。
pub fn path() -> Option<PathBuf> {
    Some(crate::config::path()?.with_file_name("config.lua"))
}

/// 走らせて、返ってきた表を TOML の形に落とす。
///
/// **落ちない。** 設定が壊れているだけで端末が開かないのは、端末エミュレータ
/// として最悪の事故（`config.rs` と同じ方針）。読めなければ理由を返して、
/// 呼ぶ側が既定へ倒す。
pub fn load(file: &Path) -> Result<toml::Value, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("読めません: {e}"))?;
    let lua = Lua::new();
    put_facts(&lua)?;
    let value: Value = lua
        .load(&text)
        .set_name(file.to_string_lossy().as_ref())
        .eval()
        .map_err(|e| trim_lua_error(&e.to_string()))?;
    match to_toml(&value)? {
        toml::Value::Table(t) => Ok(toml::Value::Table(t)),
        _ => Err("表（{ ... }）を return してください".into()),
    }
}

/// Lua から見える事実。**関数ではなく値で置く。**
///
/// `tsumugi.hostname()` のように呼ばせると、設定を書く人が「いつ呼ばれるのか」を
/// 気にすることになる。ここで決まっているものは起動時に決まっているので、
/// 値のまま置くほうが読み違えようがない。
fn put_facts(lua: &Lua) -> Result<(), String> {
    let fail = |e: mlua::Error| e.to_string();
    let t = lua.create_table().map_err(fail)?;
    t.set("os", std::env::consts::OS).map_err(fail)?;
    t.set("hostname", hostname()).map_err(fail)?;
    // 環境変数は**関数で**。全部を先に写すと、設定ファイルが
    // 秘密を持ったまま `print` されうる。
    let env = lua
        .create_function(|_, name: String| Ok(std::env::var(&name).ok()))
        .map_err(fail)?;
    t.set("env", env).map_err(fail)?;
    lua.globals().set("tsumugi", t).map_err(fail)?;
    Ok(())
}

fn hostname() -> String {
    #[cfg(windows)]
    let key = "COMPUTERNAME";
    #[cfg(not(windows))]
    let key = "HOSTNAME";
    std::env::var(key).unwrap_or_default()
}

/// Lua の値を TOML の値へ。
///
/// **表が配列かどうかは、鍵が 1..n の整数かどうかで決める。** Lua には
/// 配列と辞書の区別が無いので、ここで決めないと `[lsp.rs]` のような表が
/// 配列として渡り、設定が黙って落ちる。
fn to_toml(v: &Value) -> Result<toml::Value, String> {
    Ok(match v {
        Value::Nil => return Err("nil は設定に置けません".into()),
        Value::Boolean(b) => toml::Value::Boolean(*b),
        Value::Integer(i) => toml::Value::Integer(*i),
        Value::Number(n) => toml::Value::Float(*n),
        Value::String(s) => toml::Value::String(s.to_str().map_err(|e| e.to_string())?.to_string()),
        Value::Table(t) => {
            let len = t.raw_len();
            let is_array = len > 0
                && t.clone()
                    .pairs::<Value, Value>()
                    .filter_map(Result::ok)
                    .all(|(k, _)| matches!(k, Value::Integer(i) if i >= 1 && i as usize <= len));
            if is_array {
                let mut out = Vec::with_capacity(len);
                for i in 1..=len {
                    let item: Value = t.raw_get(i).map_err(|e| e.to_string())?;
                    out.push(to_toml(&item)?);
                }
                toml::Value::Array(out)
            } else {
                let mut out = toml::map::Map::new();
                for pair in t.clone().pairs::<Value, Value>() {
                    let (k, val) = pair.map_err(|e| e.to_string())?;
                    let key = match k {
                        Value::String(s) => s.to_str().map_err(|e| e.to_string())?.to_string(),
                        Value::Integer(i) => i.to_string(),
                        other => {
                            return Err(format!("鍵に使えない型です: {}", other.type_name()));
                        }
                    };
                    // nil は「書かなかった」と同じ。**そこで諦めない。**
                    if matches!(val, Value::Nil) {
                        continue;
                    }
                    out.insert(key, to_toml(&val)?);
                }
                toml::Value::Table(out)
            }
        }
        other => return Err(format!("設定に置けない型です: {}", other.type_name())),
    })
}

/// Lua の誤りは長い追跡を伴う。**1 行目だけ出す。**
///
/// 下の行はこちらの読み込み器の中身で、設定を書いた人には関係が無い。
fn trim_lua_error(e: &str) -> String {
    e.lines()
        .next()
        .unwrap_or(e)
        .trim()
        .trim_start_matches("runtime error: ")
        .trim_start_matches("syntax error: ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Result<toml::Value, String> {
        let lua = Lua::new();
        put_facts(&lua).unwrap();
        let v: Value = lua.load(src).eval().map_err(|e| trim_lua_error(&e.to_string()))?;
        to_toml(&v)
    }

    #[test]
    fn a_table_becomes_the_same_shape_as_the_toml() {
        let got = run("return { window = { opacity = 0.5, blur = true } }").unwrap();
        assert_eq!(
            got["window"]["opacity"].as_float(),
            Some(0.5),
            "数が落ちている"
        );
        assert_eq!(got["window"]["blur"].as_bool(), Some(true));
    }

    /// **辞書を配列と取り違えない。** `[lsp.rs]` のような表が配列として
    /// 渡ると、設定が黙って落ちる。
    #[test]
    fn a_map_with_string_keys_stays_a_map() {
        let got = run(r#"return { lsp = { rs = { command = "rust-analyzer" } } }"#).unwrap();
        assert!(got["lsp"].is_table(), "辞書が配列になっている");
        assert_eq!(got["lsp"]["rs"]["command"].as_str(), Some("rust-analyzer"));
    }

    #[test]
    fn a_list_stays_a_list() {
        let got = run(r#"return { a = { "x", "y" } }"#).unwrap();
        assert_eq!(got["a"].as_array().map(Vec::len), Some(2));
    }

    /// nil は「書かなかった」と同じ。そこで読み込みを止めない。
    #[test]
    fn a_nil_field_is_the_same_as_not_writing_it() {
        let got = run("return { a = 1, b = nil }").unwrap();
        assert_eq!(got["a"].as_integer(), Some(1));
        assert!(got.get("b").is_none());
    }

    /// 機械ごとに違うものを書ける、が Lua を入れた理由。
    #[test]
    fn the_config_can_look_at_the_machine() {
        let got = run(
            r#"
            local t = { window = {} }
            if tsumugi.os == "nosuchos" then t.window.opacity = 0.1
            else t.window.opacity = 0.9 end
            return t
        "#,
        )
        .unwrap();
        assert_eq!(got["window"]["opacity"].as_float(), Some(0.9));
    }

    /// 環境変数は関数で渡す（全部を先に写さない）。
    #[test]
    fn env_is_read_through_a_function() {
        unsafe { std::env::set_var("TSG_LUA_TEST", "yes") };
        let got = run(r#"return { a = tsumugi.env("TSG_LUA_TEST") }"#).unwrap();
        assert_eq!(got["a"].as_str(), Some("yes"));
        // 無い名前は nil。**そこで落ちない。**
        let got = run(r#"return { a = 1, b = tsumugi.env("TSG_NO_SUCH_VAR") }"#).unwrap();
        assert!(got.get("b").is_none());
    }

    /// 壊れた設定で端末が開かないのが一番悪い。理由だけ返す。
    #[test]
    fn a_broken_config_returns_a_reason_instead_of_panicking() {
        let e = run("this is not lua").unwrap_err();
        assert!(!e.is_empty());
        assert!(!e.contains('\n'), "追跡ごと出している: {e}");
    }

    #[test]
    fn returning_something_that_is_not_a_table_is_refused() {
        let lua = Lua::new();
        put_facts(&lua).unwrap();
        let v: Value = lua.load("return 3").eval().unwrap();
        let got = to_toml(&v).unwrap();
        assert!(!got.is_table(), "表でないものを表として通している");
    }
}
