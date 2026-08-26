//! 構文木。**テキストオブジェクトのためだけに持つ。**
//!
//! 強調（`syntax.rs`）はトークンの並びで足りている。木が要るのは
//! 「この関数を」「この型を」「この引数を」と**掛け算の側**を指すときで、
//! そこは行の並びをいくら眺めても出てこない。
//!
//! `concept.md` の「捨てるもの 1」で捨てたのは **Neovim の Lua / Vimscript
//! 互換**であって、tree-sitter そのものではない（同じ行に「必要なら自前で
//! 直接統合する」と書いてある）。文法（grammar）は言語中立なので、
//! Lua のプラグインと違って**そのまま使える唯一の既存資産**になる。
//!
//! ## 節の名前を言語ごとに並べない
//!
//! 文法ごとに `function_item` / `function_definition` / `method_declaration` と
//! 名前は違うが、**言葉としては同じものを指している**。言語ごとの対応表を
//! 持つと、文法を 1 つ足すたびに表も足すことになり、「文法を足すだけで
//! 言語が増える」という肝心の性質が消える。ここでは名前の**形**で見る。

use tree_sitter::{Node, Parser, Point, Tree};

/// 木で指せるもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeObject {
    /// 関数・メソッド・クロージャ（`if` / `af`）
    Function,
    /// 型・構造体・クラス・trait・impl（`it` / `at`）
    Type,
    /// 引数・仮引数の 1 つ（`ia` / `aa`）
    Argument,
}

impl TreeObject {
    /// `i` / `a` の次に来る字。
    ///
    /// `f` は端末では「ファイルパス」だが、ファイルバッファでは関数になる
    /// （`textobj.rs` が最初からそう書き残していた場所）。**同じ字で、
    /// いま見ているものに応じたいちばん近いものを指す**のが tsumugi の作法。
    pub fn of_char(c: char) -> Option<Self> {
        Some(match c {
            'f' => TreeObject::Function,
            't' => TreeObject::Type,
            'a' => TreeObject::Argument,
            _ => return None,
        })
    }

    /// その節がこれに当たるか。**名前の形で見る**（上の説明のとおり）。
    fn matches(self, kind: &str) -> bool {
        match self {
            TreeObject::Function => {
                (kind.contains("function") || kind.contains("method") || kind.contains("closure"))
                    // 呼び出しは関数ではない。`f(x)` の上で `am` が
                    // 呼び出し側を取ると、囲っている関数へ二度と届かない。
                    && !kind.contains("call")
                    && !kind.contains("type")
            }
            TreeObject::Type => {
                kind.contains("struct")
                    || kind.contains("class")
                    || kind.contains("enum")
                    || kind.contains("trait")
                    || kind.contains("interface")
                    || kind == "impl_item"
                    || kind == "type_declaration"
            }
            // 引数は「並びの子」なので、自分ではなく親を見る（`enclosing`）。
            TreeObject::Argument => false,
        }
    }
}

/// 引数の**並び**。この子が 1 つの引数になる。
///
/// 単数（`parameter`）と複数（`parameters` / `parameter_list`）を取り違えると、
/// `x: u8` ではなく `x` だけを取る。文法はどれも並びの側を複数形か
/// `_list` で名乗るので、そこで見分ける。
fn is_argument_list(kind: &str) -> bool {
    (kind.contains("parameter") || kind.contains("argument"))
        && (kind.ends_with('s') || kind.ends_with("_list"))
}

/// 対応している文法。**足すのはここ 1 か所**（依存を 1 行足して、腕を 1 つ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grammar {
    Rust,
    C,
    Python,
    JavaScript,
    Go,
    Json,
}

impl Grammar {
    /// 拡張子から。**中身は見ない**（開いた瞬間に決まってほしい）。
    pub fn of_extension(ext: &str) -> Option<Self> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "rs" => Grammar::Rust,
            "c" | "h" => Grammar::C,
            "py" | "pyi" => Grammar::Python,
            "js" | "mjs" | "cjs" | "jsx" => Grammar::JavaScript,
            "go" => Grammar::Go,
            "json" => Grammar::Json,
            _ => return None,
        })
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Grammar::Rust => tree_sitter_rust::LANGUAGE.into(),
            Grammar::C => tree_sitter_c::LANGUAGE.into(),
            Grammar::Python => tree_sitter_python::LANGUAGE.into(),
            Grammar::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Grammar::Go => tree_sitter_go::LANGUAGE.into(),
            Grammar::Json => tree_sitter_json::LANGUAGE.into(),
        }
    }
}

/// 解いた木と、それを解いたときの版。
///
/// **版で古さを見る。** バッファは変更のたびに `rev` を 1 つ進めるので、
/// どこがどう変わったかを追いかけなくても、番号 1 つで作り直す判断ができる。
pub struct Syntax {
    parser: Parser,
    tree: Option<Tree>,
    /// `tree` を作ったときの本文の版。
    rev: u64,
    /// 解いた本文そのもの。節のバイト位置を行・桁へ戻すのに要る。
    text: String,
}

impl std::fmt::Debug for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syntax")
            .field("rev", &self.rev)
            .field("parsed", &self.tree.is_some())
            .finish()
    }
}

impl Syntax {
    /// その文法で解く用意をする。**文法が無ければ持たない**
    /// （持てない言語のために空の木を配ると、呼ぶ側が毎回中を確かめる）。
    pub fn new(grammar: Grammar) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&grammar.language()).ok()?;
        Some(Self {
            parser,
            tree: None,
            rev: u64::MAX,
            text: String::new(),
        })
    }

    /// 必要なら解き直す。**同じ版なら何もしない。**
    ///
    /// 打つたびに解き直すと、大きなファイルで指が止まる。要るのは
    /// テキストオブジェクトを指した瞬間だけなので、そこまで遅らせる。
    pub fn ensure(&mut self, text: &str, rev: u64) {
        if self.rev == rev && self.tree.is_some() {
            return;
        }
        self.tree = self.parser.parse(text, None);
        self.text = text.to_string();
        self.rev = rev;
    }

    /// `at`（行・バイト桁）を囲む最小の `what`。返すのは行・バイト桁の組。
    ///
    /// `around` が真なら節そのもの、偽なら「中身」。中身は本体（`body`）が
    /// あればそこ、無ければ節と同じ。**囲みの記号を落とすためだけに
    /// 1 文字ずつ削らない**（言語ごとに囲みが違い、必ずどれかで外す）。
    pub fn object(
        &self,
        at: (usize, usize),
        what: TreeObject,
        around: bool,
    ) -> Option<((usize, usize), (usize, usize))> {
        let tree = self.tree.as_ref()?;
        let point = Point::new(at.0, at.1);
        let mut node = tree
            .root_node()
            .descendant_for_point_range(point, point)
            .unwrap_or_else(|| tree.root_node());

        if what == TreeObject::Argument {
            return self.argument(node, around);
        }

        loop {
            if what.matches(node.kind()) {
                let target = if around {
                    node
                } else {
                    node.child_by_field_name("body").unwrap_or(node)
                };
                return Some(span(target));
            }
            node = node.parent()?;
        }
    }

    /// 引数 1 つ。並びの子まで登り、その子を返す。
    ///
    /// `around` は後ろのカンマ（無ければ前のカンマ）まで取る。**取らないと
    /// `daa` の後にカンマだけが残る**ので、消したのに構文が壊れる。
    fn argument(&self, from: Node, around: bool) -> Option<((usize, usize), (usize, usize))> {
        let mut node = from;
        loop {
            let parent = node.parent()?;
            if is_argument_list(parent.kind()) && node.is_named() {
                if !around {
                    return Some(span(node));
                }
                let mut end = node.end_position();
                let mut start = node.start_position();
                // 後ろのカンマを飲む。無ければ前のカンマを飲む。
                let mut sib = node.next_sibling();
                while let Some(s) = sib {
                    if s.kind() == "," {
                        end = s.end_position();
                        break;
                    }
                    if s.is_named() {
                        break;
                    }
                    sib = s.next_sibling();
                }
                if end == node.end_position() {
                    let mut prev = node.prev_sibling();
                    while let Some(s) = prev {
                        if s.kind() == "," {
                            start = s.start_position();
                            break;
                        }
                        if s.is_named() {
                            break;
                        }
                        prev = s.prev_sibling();
                    }
                }
                return Some(((start.row, start.column), (end.row, end.column)));
            }
            node = parent;
        }
    }
}

fn span(node: Node) -> ((usize, usize), (usize, usize)) {
    let s = node.start_position();
    let e = node.end_position();
    ((s.row, s.column), (e.row, e.column))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syn(g: Grammar, text: &str) -> Syntax {
        let mut s = Syntax::new(g).expect("文法を読めない");
        s.ensure(text, 1);
        s
    }

    /// 節の名前を言語ごとに並べていないことの確かめ。
    /// **同じ字（`am`）が、対応表を足さずに 3 つの文法で効く。**
    #[test]
    fn the_same_object_works_across_grammars() {
        let cases = [
            (Grammar::Rust, "fn a() {\n    let x = 1;\n}\n", 1usize),
            (Grammar::Python, "def a():\n    x = 1\n", 1usize),
            (Grammar::Go, "func a() {\n\tx := 1\n}\n", 1usize),
        ];
        for (g, src, line) in cases {
            let s = syn(g, src);
            let (start, _) = s
                .object((line, 4), TreeObject::Function, true)
                .unwrap_or_else(|| panic!("{g:?} で関数を取れない"));
            assert_eq!(start.0, 0, "{g:?} で関数の頭を指していない");
        }
    }

    #[test]
    fn an_inner_function_is_its_body() {
        let src = "fn a() {\n    let x = 1;\n}\n";
        let s = syn(Grammar::Rust, src);
        let (around, _) = s.object((1, 4), TreeObject::Function, true).unwrap();
        let (inner, _) = s.object((1, 4), TreeObject::Function, false).unwrap();
        assert_eq!(around.0, 0, "外側は fn の行から");
        assert_eq!(inner.0, 0, "本体は同じ行の `{{` から");
        assert!(inner.1 > around.1, "本体のほうが右から始まるはず");
    }

    #[test]
    fn a_call_is_not_a_function() {
        // `f(x)` の上で `am` が呼び出しを取ると、囲っている関数へ届かなくなる。
        let src = "fn a() {\n    g(1);\n}\n";
        let s = syn(Grammar::Rust, src);
        let (start, end) = s.object((1, 5), TreeObject::Function, true).unwrap();
        assert_eq!(start.0, 0);
        assert_eq!(end.0, 2, "呼び出しのほうを取っている");
    }

    #[test]
    fn a_type_is_found_from_inside() {
        let src = "struct S {\n    a: u8,\n}\n";
        let s = syn(Grammar::Rust, src);
        let (start, end) = s.object((1, 4), TreeObject::Type, true).unwrap();
        assert_eq!((start.0, end.0), (0, 2));
    }

    #[test]
    fn around_an_argument_takes_the_comma_with_it() {
        // カンマを残すと、消したのに構文が壊れる。
        let src = "fn a(x: u8, y: u8) {}\n";
        let s = syn(Grammar::Rust, src);
        let (istart, iend) = s.object((0, 5), TreeObject::Argument, false).unwrap();
        assert_eq!(&src[istart.1..iend.1], "x: u8");
        let (astart, aend) = s.object((0, 5), TreeObject::Argument, true).unwrap();
        assert_eq!(&src[astart.1..aend.1], "x: u8,");
    }

    #[test]
    fn the_last_argument_takes_the_comma_before_it() {
        let src = "fn a(x: u8, y: u8) {}\n";
        let s = syn(Grammar::Rust, src);
        let (astart, aend) = s.object((0, 13), TreeObject::Argument, true).unwrap();
        assert_eq!(&src[astart.1..aend.1], ", y: u8");
    }

    #[test]
    fn nothing_to_point_at_is_none_not_a_guess() {
        let src = "let x = 1;\n";
        let s = syn(Grammar::Rust, src);
        assert!(s.object((0, 2), TreeObject::Function, true).is_none());
    }

    #[test]
    fn parsing_again_at_the_same_revision_is_skipped() {
        let mut s = Syntax::new(Grammar::Rust).unwrap();
        s.ensure("fn a() {}\n", 1);
        s.ensure("fn b() {}\n", 1); // 版が同じなら見直さない
        assert!(s.object((0, 3), TreeObject::Function, true).is_some());
        assert_eq!(s.text, "fn a() {}\n", "版が同じなのに解き直している");
        s.ensure("fn b() {}\n", 2);
        assert_eq!(s.text, "fn b() {}\n", "版が進んだのに解き直していない");
    }
}
