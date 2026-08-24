# tsumugi shell integration (fish)
#
# 何をするか: プロンプトとコマンドの境目を OSC 133 で、作業ディレクトリを OSC 7 で知らせる。
# これが無いと tsumugi の [[ ]] [e ]e ・左ガター・`ac` / `io` が効かない。
#
# 入れ方:  tsg --shell-integration fish | source   を ~/.config/fish/config.fish に足す

if set -q TSG_SHELL_INTEGRATION
    exit 0
end
set -g TSG_SHELL_INTEGRATION 1
set -g __tsg_running 0

function __tsg_url
    string replace -a ' ' '%20' -- $argv[1] |
        string replace -a '#' '%23' |
        string replace -a '?' '%3F'
end

function __tsg_prompt_start --on-event fish_prompt
    printf '\033]7;file://%s%s\007' (hostname) (__tsg_url $PWD)
    printf '\033]133;A\007'
end

function __tsg_preexec --on-event fish_preexec
    set -g __tsg_running 1
    printf '\033]133;C\007'
end

function __tsg_postexec --on-event fish_postexec
    printf '\033]133;D;%s\007' $status
    set -g __tsg_running 0
end

# プロンプトの終わり（B）。既存の fish_prompt を包む。
if not functions -q __tsg_orig_fish_prompt
    functions -c fish_prompt __tsg_orig_fish_prompt
    function fish_prompt
        __tsg_orig_fish_prompt
        printf '\033]133;B\007'
    end
end
