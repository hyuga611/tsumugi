# tsumugi

> **The terminal screen is a document. Read it, select it, edit it — with vim
> motions or with the mouse. Every command has both.**

A terminal emulator that treats scrollback as a buffer you can navigate, and
that knows when the AI agent running inside it is waiting for you.

Rust. Windows today; macOS and Linux build but are **not yet tested** (see
[Status](#status)).

日本語の説明は [README.ja.md](README.ja.md) にあります。

![tsumugi](assets/demo.gif)

## Why another terminal

**1. The scrollback is a document, not a log.** Move with `j` `k` `w` `[[` `]]`,
select a whole command block with `ac`, yank it, pipe it through `jq`, open a
file in the same pane with `:e`. The grid and a file buffer are the same thing
to the editor, so the same keys work on both.

**2. Every command is reachable with the mouse — enforced in CI.** A test walks
the command registry and fails if any command lacks a mouse path. Double-click
selects a path or URL as one token. The left gutter marks each command's exit
code; click it to select that command and its output. Right-click shows what you
can do *here*.

**3. It knows what your agents are doing.** Run Claude Code or Codex in three
panes; the tab shows ● when one is waiting for you, and `Space a` jumps there.
Send one prompt to all of them (`Space b`) and compare the answers
(`tsg --compare`). The state is **reported by the agent's own hooks**, not
guessed from the screen — guessing breaks silently the day the output changes.

Closing the window does not kill your shells: the multiplexer is a separate
process, so reopening puts you back where you were, including files you had
open and had not saved.

## Install

```
git clone https://github.com/hyuga611/tsumugi
cd tsumugi
cargo build --release
./target/release/tsg --install
```

`--install` adds a Start Menu and desktop shortcut, puts `tsg` on your PATH, and
adds "Open tsumugi here" to the folder context menu. **It does not move the
executable** — it points at wherever you built it. `tsg --uninstall` removes
everything it added, and every change is printed as it happens.

Strongly recommended, once:

```
tsg --install-shell-integration     # OSC 133: prompt marks, exit codes, command blocks
```

Without it, `[[` `]]`, the gutter, and `ac` / `io` have nothing to work from.

> **Windows SmartScreen**: the binary is unsigned, so the first launch shows
> "Windows protected your PC". More info → Run anyway. If Smart App Control is
> on, it will block the binary outright; there is no workaround short of turning
> that feature off, which is irreversible.

## The first five minutes

Open it and press **F1**. The help starts with what the mouse alone can do.

| | |
|---|---|
| type | it is a normal terminal |
| `Esc` | reading mode — the bar at the bottom changes colour |
| click the bar | toggles typing ⇄ reading without knowing any keys |
| double-click | select a word, path or URL as one |
| `Ctrl`+click | open that path or URL |
| right-click | everything you can do here |
| `≡` at the bottom | every command, searchable |

## What it does

**Reading and editing** — vim motions over scrollback, text objects
(`ac` command block, `io` output, `if` path, `iu` URL, `ih` hash), operators
(`d` `c` `y` `=` `>`), marks, macros, registers, undo/redo.
`:e` turns the pane into an editor; `:w` saves; `:q` goes back to the shell.

**Finding things** — `/` searches as you type and highlights every match.
`Space l` labels every path and URL on screen so one keypress opens it.
`Space o` folds a command's output; the folded line says what it hid.

**Panes and sessions** — split, zoom, swap, resize, tabs, named sessions,
detach and reattach. `Space S` lists what is running. Closing the window
leaves your shells and agents running. **Losing the machine doesn't: the
layout and each pane's directory come back on the next launch** (the screen
contents are never written to disk). For an agent, the resume line is placed
at the prompt — pressing it is your call.

**Language servers (LSP)** — errors are underlined with a squiggle and `[e`
`]e` walk them; `gd` goes to the definition, Ctrl+Space completes. It is
**use-it-if-you-have-it**: with no language server installed, nothing happens
(you just get no diagnostics). Defaults cover rust-analyzer, gopls, pyright,
typescript-language-server and clangd; add more under `[lsp.<ext>]`.

**Remote** — list a host under `[domains]` and open it with `tsg -d <name>`
(it also shows up in `Space S`). **Losing the link doesn't lose the session**:
the far side keeps running and reattaching brings the screen back. tsumugi has
to be installed on the far side; keys and jump hosts are left to
`~/.ssh/config`.

**Reading output** — syntax highlighting, `git diff` in colour (`Space g`),
Markdown rendered in place (`Space m`), images (Kitty graphics and Sixel),
OSC 8 hyperlinks, a position indicator on the right edge. Narrowing the window
**re-wraps** the scrollback instead of cutting it off.

**For AI agents** — see [For agents](#for-agents).

**Looks** — three themes plus per-colour overrides, ligatures, a translucent
blurred background by default, Japanese/English UI, IME that follows the mode.

## For agents

```
tsg --install-agent-hooks          # wire Claude Code / Codex, once
```

The agent then reports its own state, and tsumugi shows it:

| | |
|---|---|
| `●` on a tab | waiting for you |
| `✓` / `✕` | finished / failed |
| `● waiting N` at the bottom | click to jump there |
| `Space a` | jump to the next one waiting |
| taskbar flash | only when the window is in the background, only on change |

Scriptable, and it answers with exit codes so you can put it in an `if`:

```
tsg --agents                          # session <TAB> pane <TAB> state
tsg --prompt "fix the failing test" --wait
tsg --wait --until done --timeout 600
tsg --broadcast "review this diff" --wait   # every visible pane
tsg --compare                               # their answers, side by side
tsg --layout agents                         # three panes
```

## Configuration

`%APPDATA%\tsumugi\config.toml` (`~/.config/tsumugi/config.toml` on Unix).
It works without one; a broken one starts with defaults and a warning rather
than refusing to open. **Menu → Open the config file** creates a commented
template with every setting and its default.

```toml
[ui]
lang = "auto"                 # "ja" / "en" / "auto"

[window]
opacity = 0.85
blur = true

[font]
size = 18.0
family = "Cascadia Code"      # falls back through the stack if absent
ligatures = true
ambiguous_width = "narrow"    # "wide" for the older CJK convention

[theme]
name = "yogiri"               # yogiri / sumi / hakuji

[keys]
"ctrl+k" = "search.open"      # any command id — `tsg --commands` lists them
"F5"     = "git.diff"

[keys.insert]
"ctrl+g" = "agent.next"       # Ctrl or F keys only while typing
```

Saving takes effect immediately. **Your bindings are layered on top of the
defaults**, so keys you do not mention keep working.

## Driving it from outside

The multiplexer speaks JSON Lines over a socket that is closed to everyone but
you. Convenience commands are wrappers; `--rpc` is the escape hatch.

```
tsg --list                     # running sessions
tsg --capture                  # what a pane shows, as text
tsg --open README.md --render  # open a file in the running window
tsg --search "TODO"            # search from outside; n / N still work
tsg --run <command-id>         # any command in the UI (--commands lists them)
tsg --rpc                      # raw protocol on stdin/stdout — see docs/rpc.md
```

## Status

**Windows** is developed and tested on. Everything in this README was verified
on a real machine.

**macOS and Linux** compile, and the terminal, multiplexer, editor and modal
layers are platform-independent — but the window decoration, IME, and
`--install` are written against Windows APIs, and **nobody has run it there
yet**. Treat those platforms as untested rather than supported.

Not done yet: cross-line syntax highlighting. Ligatures
work but could not be verified here (no ligature font on the development
machine; `tsg --diagnose` will tell you).

## Security

The multiplexer socket is restricted to your user, and tsumugi checks who is on
the other end before trusting it. Scrollback lives only in the multiplexer's
memory — **it is never written to disk**. Terminal escape sequences are treated
as untrusted input: OSC 52 clipboard writes are accepted but reads are refused,
`file://` cwd from another host is rejected, and image payloads are bounded.

Details and how to report a problem: [SECURITY.md](SECURITY.md).

## Design

The four design documents are the source of truth, and predate the code:

- [docs/concept.md](docs/concept.md) — the central claim and what follows from it
- [docs/modal-spec.md](docs/modal-spec.md) — the modal layer
- [docs/mouse-parity.md](docs/mouse-parity.md) — every command's mouse path
- [docs/arch.md](docs/arch.md) — architecture, invariants, milestones

Each milestone has a results document (`docs/m*-results.md`) recording what was
measured on real hardware, including what went wrong.

## License

MIT OR Apache-2.0.
