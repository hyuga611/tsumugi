# tsumugi shell integration (nushell)
#
# 何をするか: プロンプトとコマンドの境目を OSC 133 で、作業ディレクトリを OSC 7 で知らせる。
# これが無いと tsumugi の [[ ]] [e ]e ・左ガター・`ac` / `io` が効かない。
#
# 入れ方:  tsg --shell-integration nu | save -f ~/.config/nushell/tsumugi.nu
#          source ~/.config/nushell/tsumugi.nu   を config.nu に足す

$env.config = ($env.config | upsert hooks {
    pre_prompt: [{||
        # 直前のコマンドの終わり。nu は $env.LAST_EXIT_CODE を持っている。
        let code = ($env.LAST_EXIT_CODE? | default 0)
        print -n $"(ansi -e ']133;D;')($code)(char bel)"
        let p = ($env.PWD | str replace -a ' ' '%20')
        print -n $"(ansi -e ']7;file://')($p)(char bel)"
        print -n $"(ansi -e ']133;A')(char bel)"
    }]
    pre_execution: [{||
        print -n $"(ansi -e ']133;C')(char bel)"
    }]
})

# プロンプトの終わり（B）は右プロンプトの手前に置く。
$env.PROMPT_COMMAND_RIGHT = {||
    $"(ansi -e ']133;B')(char bel)"
}

# ---------------------------------------------------------------------------
# `go` — 作業台を組む
#
# cd してから `go` と打つと、いまのペインを真ん中にして左にディレクトリの木、
# 右に AI エージェントが並び、タブの名前がそのディレクトリになる。
#
# Go 言語の `go` と名前がぶつかるので、引数があれば本物（`^go`）へ渡す。
export def --wrapped go [...args] {
    if ($args | is-empty) and ($env.TSUMUGI_SESSION? | is-not-empty) {
        tsg --workspace $env.PWD
    } else {
        ^go ...$args
    }
}
