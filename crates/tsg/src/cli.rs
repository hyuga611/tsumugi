//! コマンドラインの入口。
//!
//! ターミナルエミュレータは**他のプログラムから起動される**ことが多い。
//! ファイラの「ここでターミナルを開く」、エディタのタスク実行、ランチャ、
//! ショートカット。したがって `--cwd` と `-e` は飾りではなく既定の使われ方の一部で、
//! ここが無いと「起動はできるが常用はできない」ものになる。

use std::path::PathBuf;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// 通常起動（GUI）
    Run,
    /// mux サーバとして走る（GUI から自動で起こされる）
    Server,
    /// 走っているセッションへ外から入力を流す
    Send(String),
    /// セッションの生バイトを覗く
    Tap,
    /// 走っているセッションの一覧。
    List,
    /// ペインに見えているものをテキストで取る。
    Capture(Option<u32>),
    /// 生のプロトコルを標準入出力で話す。
    Rpc,
    /// フォントと CJK 幅の数値だけ出して終了
    Diagnose,
    /// シェル統合のスクリプトを標準出力へ出す（`eval` で読ませる用）
    ShellIntegration(Option<String>),
    /// シェル統合を置いて rc に 1 行足す
    InstallShellIntegration(Option<String>),
    /// エージェントが自分の状態を名乗る（hooks から呼ばれる）
    AgentState(String),
    /// どのペインのエージェントがどうなっているか
    Agents,
    /// その状態になるまで待つ（台本用）
    Wait { until: String, timeout: u64 },
    /// エージェントへ文を投げる。`--wait` を付けると返事待ちになるまで待つ
    Prompt { text: String, wait: bool },
    /// エージェントの hooks を入れる / 外す
    InstallAgentHooks(Option<String>),
    UninstallAgentHooks(Option<String>),
    /// スタートメニュー・PATH・右クリックメニューへ登録する
    Install,
    /// それを全部外す
    Uninstall,
    Help,
    Version,
}

#[derive(Clone, Debug)]
pub struct Cli {
    pub mode: Mode,
    pub session: String,
    /// 新しいペインを開く作業ディレクトリ。
    pub cwd: Option<PathBuf>,
    /// シェルの代わりに走らせるもの（`-e`）。
    pub command: Option<Vec<String>>,
    pub opacity: Option<f32>,
    pub blur: Option<bool>,
    pub font_size: Option<f32>,
    /// 表示の言語（`ja` / `en`）。設定より優先する。
    pub lang: Option<String>,
    /// テーマの名前。設定より優先する。
    pub theme: Option<String>,
    /// `--session` を明示されたか。`-e` のときの既定を変えるのに使う。
    pub session_given: bool,
    /// 相手のペイン。書かなければ「いま選ばれているペイン」。
    pub pane: Option<u32>,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            mode: Mode::Run,
            session: "default".into(),
            cwd: None,
            command: None,
            opacity: None,
            blur: None,
            font_size: None,
            lang: None,
            theme: None,
            session_given: false,
            pane: None,
        }
    }
}

pub const HELP: &str = "\
tsumugi (tsg) — ターミナルの画面を vim で編集できるドキュメントとして扱うターミナル

使い方:
  tsg [オプション]
  tsg [オプション] -e <コマンド> [引数...]

オプション:
  -e, --command <cmd...>   シェルの代わりにコマンドを走らせる（以降すべてを引数として取る）
      --cwd <dir>          そのディレクトリでシェルを開く（既定: 起動したディレクトリ）
  -s, --session <名前>     名前付きセッション（既定: default）
      --opacity <0.0-1.0>  ウィンドウの不透明度
      --no-blur            背景のぼかしを切る
      --font-size <px>     文字の大きさ
      --lang <ja|en>       表示の言語（既定: OS に合わせる）
      --theme <名前>       配色（夜霧 / 墨 / 白磁。英名 yogiri / sumi / hakuji でも可）
      --install            スタートメニュー / デスクトップ / PATH /
                           フォルダの右クリックに登録する（exe は動かさない）
      --uninstall          それを全部外す
      --shell-integration [シェル]
                           シェル統合（OSC 133）のスクリプトを出す
      --install-shell-integration [シェル]
                           それを置いて、シェルの設定ファイルに 1 行足す
      --diagnose           フォントと CJK 幅の実測値を出して終了
      --list               走っているセッションを並べる
      --send <文字列>      走っているセッションへ入力を流す（\\n で改行）
      --capture [ペイン]   ペインに見えているものをテキストで取る（既定: いまのペイン）
      --tap                そのセッションの生バイトを覗く
      --rpc                生のプロトコルを標準入出力で話す（docs/rpc.md）
  -h, --help               これ
  -V, --version            版

設定ファイル:
  ~/.config/tsumugi/config.toml（Windows は %APPDATA%\\tsumugi\\config.toml）
  コマンドラインの指定が設定ファイルより優先される。

  [window]
  opacity = 0.92
  blur = true

  [font]
  size = 18.0
  ligatures = true        # -> や != を 1 つの字形に組む

  [scrollback]
  lines = 10000

  [theme]
  name = \"夜霧\"           # 夜霧 / 墨 / 白磁

  [theme.colors]          # 個別に上書き（省略可）
  background = \"#0f1217\"
  ansi1 = \"#e05a63\"

はじめに（1 回だけ）:
  tsg --install                          # スタートメニュー・PATH・右クリックに登録
  tsg --install-shell-integration        # プロンプトの位置を伝える設定を入れる

  以後は `tsg` と打つか、スタートメニュー / デスクトップのアイコンから開く。
  フォルダを右クリックして「tsumugi でここを開く」でも開く。

  使い方の画面は初めて開いたときだけ全画面で出る。あとは F1 か、
  下の「? 使い方」から。

AI エージェントを並べて使うなら:
  何本も走らせると「どれが返事待ちか」を目で探す時間が仕事の大半になる。
  エージェント自身に名乗らせて、タブの印・下の「返事待ち N」・Space a で消す。

    tsg --install-agent-hooks              # Claude Code / Codex に配線する
    Space a                                # 次の返事待ちへ飛ぶ
    Space f                                # 画面に出てきたファイルの一覧
    [a  ]a                                 # 前 / 次の発話へ

  台本から回すこともできる:

    tsg --prompt \"テストを直して\" --wait  # 投げて、返事待ちになるまで待つ
    tsg --wait --until done --timeout 600  # 終わるまで待つ
    tsg --agents                           # session<TAB>pane<TAB>state

シェル統合（強く推奨）:
  プロンプトの位置と終了コードを OSC 133 で知らせる設定。これが無いと
  [[ ]] [e ]e ・左ガター・ac / io（コマンドブロック）が効かない。

    tsg --install-shell-integration        # 今のシェルを見て入れる
    tsg --shell-integration bash           # 中身だけ見る / 自分で読ませる

  対応は bash / zsh / fish / pwsh / nu。cmd.exe には同じ口が無い。

セッションについて:
  ウィンドウを閉じてもシェルは死なない。同じ名前でもう一度起動すると続きから使える。
  終わらせるときは配置モード（Space）で Q。
";

/// 次の引数を値として取る。**`-` で始まるものは取らない**
/// （`--shell-integration` の後ろに何も書かずに `--session x` と続けられるように）。
fn next_value(args: &[String], i: &mut usize) -> Option<String> {
    let v = args.get(*i + 1)?;
    if v.starts_with('-') {
        return None;
    }
    *i += 1;
    Some(v.clone())
}

/// 引数を解釈する。未知の引数はエラーにせず、そのまま無視して起動する
/// （ランチャが余計なものを付けても端末が開かない、を避ける）。
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Cli {
    let args: Vec<String> = args.into_iter().collect();
    let mut cli = Cli::default();
    let mut i = 0;

    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match a {
            "--server" => {
                cli.mode = Mode::Server;
                if let Some(v) = next(&mut i) {
                    cli.session = v;
                    cli.session_given = true;
                }
            }
            "-s" | "--session" => {
                if let Some(v) = next(&mut i) {
                    cli.session = v;
                    cli.session_given = true;
                }
            }
            "--cwd" => cli.cwd = next(&mut i).map(PathBuf::from),
            "-e" | "--command" => {
                // 以降はすべてコマンドの引数。ここで打ち切る。
                let rest: Vec<String> = args[i + 1..].to_vec();
                if !rest.is_empty() {
                    cli.command = Some(rest);
                }
                break;
            }
            "--" => {
                let rest: Vec<String> = args[i + 1..].to_vec();
                if !rest.is_empty() {
                    cli.command = Some(rest);
                }
                break;
            }
            "--opacity" => cli.opacity = next(&mut i).and_then(|v| v.parse().ok()),
            "--no-blur" => cli.blur = Some(false),
            "--blur" => cli.blur = Some(true),
            "--font-size" => cli.font_size = next(&mut i).and_then(|v| v.parse().ok()),
            "--lang" => cli.lang = next_value(&args, &mut i),
            "--theme" => cli.theme = next_value(&args, &mut i),
            "--shell-integration" => {
                cli.mode = Mode::ShellIntegration(next_value(&args, &mut i));
            }
            "--install-shell-integration" => {
                cli.mode = Mode::InstallShellIntegration(next_value(&args, &mut i));
            }
            "--install" => cli.mode = Mode::Install,
            "--uninstall" => cli.mode = Mode::Uninstall,
            "--diagnose" => cli.mode = Mode::Diagnose,
            "--tap" => cli.mode = Mode::Tap,
            "--list" => cli.mode = Mode::List,
            "--rpc" => cli.mode = Mode::Rpc,
            "--pane" => {
                cli.pane = next_value(&args, &mut i).and_then(|v| v.parse().ok());
            }
            "--agents" => cli.mode = Mode::Agents,
            "--agent-state" => {
                let v = next_value(&args, &mut i).unwrap_or_default();
                cli.mode = Mode::AgentState(v);
            }
            "--install-agent-hooks" => {
                cli.mode = Mode::InstallAgentHooks(next_value(&args, &mut i));
            }
            "--uninstall-agent-hooks" => {
                cli.mode = Mode::UninstallAgentHooks(next_value(&args, &mut i));
            }
            "--until" => {
                let until = next_value(&args, &mut i).unwrap_or_else(|| "blocked".into());
                match &mut cli.mode {
                    Mode::Wait { until: u, .. } => *u = until,
                    Mode::Prompt { wait, .. } => *wait = true,
                    _ => cli.mode = Mode::Wait { until, timeout: 0 },
                }
            }
            "--timeout" => {
                let t = next_value(&args, &mut i).and_then(|v| v.parse().ok()).unwrap_or(0);
                if let Mode::Wait { timeout, .. } = &mut cli.mode {
                    *timeout = t;
                }
            }
            "--wait" => match &mut cli.mode {
                Mode::Prompt { wait, .. } => *wait = true,
                Mode::Wait { .. } => {}
                _ => {
                    cli.mode = Mode::Wait {
                        until: "blocked".into(),
                        timeout: 0,
                    }
                }
            },
            "--prompt" => {
                let text = next_value(&args, &mut i).unwrap_or_default();
                cli.mode = Mode::Prompt { text, wait: false };
            }
            "--capture" => {
                cli.mode = Mode::Capture(next_value(&args, &mut i).and_then(|v| v.parse().ok()));
            }
            "--send" => {
                let rest = args[i + 1..].join(" ");
                cli.mode = Mode::Send(rest);
                break;
            }
            "-h" | "--help" => {
                cli.mode = Mode::Help;
                return cli;
            }
            "-V" | "--version" => {
                cli.mode = Mode::Version;
                return cli;
            }
            _ => {}
        }
        i += 1;
    }

    // `-e` は「その場で何かを走らせる」用途なので、既存セッションへ相乗りしない。
    // 名前を明示されていれば従う。
    if cli.command.is_some() && !cli.session_given {
        cli.session = format!("run-{}", std::process::id());
    }
    cli
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(s: &[&str]) -> Cli {
        parse(s.iter().map(|x| (*x).to_string()))
    }

    #[test]
    fn defaults_are_a_plain_gui_launch() {
        let c = cli(&[]);
        assert_eq!(c.mode, Mode::Run);
        assert_eq!(c.session, "default");
        assert!(c.command.is_none());
    }

    #[test]
    fn dash_e_takes_everything_after_it() {
        // `-e cargo test --workspace` の `--workspace` を tsg の引数として食わない
        let c = cli(&["-e", "cargo", "test", "--workspace", "--help"]);
        assert_eq!(
            c.command.as_deref(),
            Some(&["cargo".to_string(), "test".into(), "--workspace".into(), "--help".into()][..])
        );
        assert_eq!(c.mode, Mode::Run, "-e の後の --help を拾ってしまっている");
    }

    #[test]
    fn dash_e_gets_its_own_session_unless_named() {
        let c = cli(&["-e", "top"]);
        assert!(c.session.starts_with("run-"), "既存セッションへ相乗りしている");

        let named = cli(&["--session", "work", "-e", "top"]);
        assert_eq!(named.session, "work", "明示した名前を無視している");
    }

    #[test]
    fn double_dash_also_ends_the_options() {
        let c = cli(&["--cwd", "/tmp", "--", "sh", "-c", "echo hi"]);
        assert_eq!(c.cwd, Some(PathBuf::from("/tmp")));
        assert_eq!(c.command.as_ref().map(Vec::len), Some(3));
    }

    #[test]
    fn window_options_are_parsed() {
        let c = cli(&["--opacity", "0.8", "--no-blur", "--font-size", "20"]);
        assert_eq!(c.opacity, Some(0.8));
        assert_eq!(c.blur, Some(false));
        assert_eq!(c.font_size, Some(20.0));
    }

    #[test]
    fn unknown_arguments_do_not_stop_the_terminal_from_opening() {
        // ランチャが余計なものを付けても端末は開くべき
        let c = cli(&["--what-is-this", "-x"]);
        assert_eq!(c.mode, Mode::Run);
    }

    #[test]
    fn shell_integration_takes_an_optional_name() {
        assert_eq!(
            cli(&["--shell-integration", "bash"]).mode,
            Mode::ShellIntegration(Some("bash".into()))
        );
        // 名前を省いたら今のシェルを見る。次の引数を食わない。
        let c = cli(&["--shell-integration", "--session", "x"]);
        assert_eq!(c.mode, Mode::ShellIntegration(None));
        assert_eq!(c.session, "x", "後ろのオプションを食っている");
    }

    #[test]
    fn help_and_version_win_over_everything() {
        assert_eq!(cli(&["--session", "x", "-h"]).mode, Mode::Help);
        assert_eq!(cli(&["-V"]).mode, Mode::Version);
    }
}
