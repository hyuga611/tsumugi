# tsumugi shell integration (zsh)
#
# 何をするか: プロンプトとコマンドの境目を OSC 133 で、作業ディレクトリを OSC 7 で知らせる。
# これが無いと tsumugi の [[ ]] [e ]e ・左ガター・`ac` / `io` が効かない。
#
# 入れ方:  eval "$(tsg --shell-integration zsh)"   を ~/.zshrc に足す

if [[ -n "${TSG_SHELL_INTEGRATION:-}" ]]; then
    return 0
fi
TSG_SHELL_INTEGRATION=1

__tsg_url() {
    local s=$1 out= i c
    for (( i = 1; i <= ${#s}; i++ )); do
        c=${s[i]}
        case $c in
            ' ') out+='%20' ;;
            '#') out+='%23' ;;
            '?') out+='%3F' ;;
            '%') out+='%25' ;;
            *) out+=$c ;;
        esac
    done
    print -n -- "$out"
}

__tsg_precmd() {
    local st=$?
    if [[ -n "${__tsg_running:-}" ]]; then
        printf '\033]133;D;%s\007' "$st"
        __tsg_running=
    fi
    printf '\033]7;file://%s%s\007' "${HOST:-}" "$(__tsg_url "$PWD")"
    printf '\033]133;A\007'
}

__tsg_preexec() {
    __tsg_running=1
    printf '\033]133;C\007'
}

# プロンプトの終わり（B）。`%{...%}` で幅 0 と伝えないと桁がずれる。
if [[ "$PS1" != *'133;B'* ]]; then
    PS1="${PS1}%{$(printf '\033]133;B\007')%}"
fi

autoload -Uz add-zsh-hook
add-zsh-hook precmd __tsg_precmd
add-zsh-hook preexec __tsg_preexec

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
