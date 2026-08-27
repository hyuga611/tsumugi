# tsumugi shell integration (bash)
#
# 何をするか: プロンプトとコマンドの境目を OSC 133 で、作業ディレクトリを OSC 7 で知らせる。
# これが無いと tsumugi の [[ ]] [e ]e ・左ガター・`ac` / `io`（コマンドブロック）が
# 「プロンプトがどこか分からない」状態になり、製品の中核語彙が丸ごと効かなくなる。
#
# 入れ方:  eval "$(tsg --shell-integration bash)"   を ~/.bashrc に足す
#          （tsg --install-shell-integration bash が代わりにやる）

# 二重に入れない。source を 2 回踏んでも壊れないこと。
if [[ -n "${TSG_SHELL_INTEGRATION:-}" ]]; then
    return 0 2>/dev/null || true
fi
TSG_SHELL_INTEGRATION=1

# パスの URL 化。空白と `#` `?` `%` だけを外す。
# 日本語などのマルチバイトはそのまま通す（受け側がパーセントデコードするので、
# ここでバイト単位に潰すと元へ戻せなくなる）。
__tsg_url() {
    local s=$1 out= i c
    for ((i = 0; i < ${#s}; i++)); do
        c=${s:i:1}
        case $c in
            ' ') out+='%20' ;;
            '#') out+='%23' ;;
            '?') out+='%3F' ;;
            '%') out+='%25' ;;
            *) out+=$c ;;
        esac
    done
    printf '%s' "$out"
}

__tsg_precmd() {
    local st=$?
    # 直前のコマンドの終わり（初回は出さない）
    if [[ -n "${__tsg_running:-}" ]]; then
        printf '\033]133;D;%s\007' "$st"
        __tsg_running=
    fi
    printf '\033]7;file://%s%s\007' "${HOSTNAME:-}" "$(__tsg_url "$PWD")"
    printf '\033]133;A\007'
    return $st
}

__tsg_preexec() {
    __tsg_running=1
    printf '\033]133;C\007'
}

# プロンプトの終わり（B）は PS1 の末尾に置く。ここから右がユーザーの入力。
case "$PS1" in
    *'\033]133;B'*) ;;
    *) PS1="${PS1}\[\033]133;B\007\]" ;;
esac

PROMPT_COMMAND="__tsg_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"

# コマンド開始（C）は DEBUG トラップで拾う。既存のトラップは壊さない。
__tsg_debug() {
    [[ -n "${COMP_LINE:-}" ]] && return                 # 補完中は無視
    [[ "$BASH_COMMAND" == "__tsg_precmd"* ]] && return  # 自分自身は無視
    [[ -n "${__tsg_running:-}" ]] && return             # 1 コマンドにつき 1 回
    __tsg_preexec
}
trap '__tsg_debug' DEBUG

# ---------------------------------------------------------------------------
# `go` — 作業台を組む
#
# cd してから `go` と打つと、いまのペインを真ん中にして左にディレクトリの木、
# 右に AI エージェントが並び、タブの名前がそのディレクトリになる。
#
# Go 言語の `go` と名前がぶつかるので、**引数があれば本物へ渡す**
# （`go build` はそのまま通る）。tsumugi の外でも本物へ渡す。
# 名前を取られたくなければ、これを読む前に TSUMUGI_NO_GO=1 を置く。
__tsg_real_go() {
    _tsg_ifs=$IFS
    IFS=:
    for _tsg_d in $PATH; do
        if [ -x "$_tsg_d/go" ]; then
            IFS=$_tsg_ifs
            printf '%s\n' "$_tsg_d/go"
            return 0
        fi
    done
    IFS=$_tsg_ifs
    return 1
}

if [ -z "${TSUMUGI_NO_GO:-}" ]; then
    go() {
        if [ "$#" -eq 0 ] && [ -n "${TSUMUGI_SESSION:-}" ]; then
            tsg --workspace "$PWD"
            return
        fi
        if _tsg_go=$(__tsg_real_go); then
            "$_tsg_go" "$@"
        else
            printf 'go: %s\n' "${1:-}" >&2
            return 127
        fi
    }
fi
