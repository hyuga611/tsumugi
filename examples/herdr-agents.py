#!/usr/bin/env python3
"""herdr のエージェントを tsumugi の中で見る — 拡張の実例。

`docs/rpc.md` §5 の口を、ひととおり使ってみせるための 1 本。
名乗り（`ext_hello`）・語彙を足す（`register_command`）・押されたら受け取る
（`subscribe`）・自分のペインを持つ（`ext_pane_open` / `write`）・画面へ
知らせる（`notify`）を、全部通る道で使っている。

    py examples/herdr-agents.py            # 既定のセッションへ
    py examples/herdr-agents.py -s work

繋いだら、tsumugi のコマンドパレットに「herdr のエージェント」が出る。
押すとペインが開いて、herdr が抱えているエージェントが並ぶ。誰かが
返事待ち（blocked）になったら、ステータス行へ知らせが出る。

## なぜ CLI 越しなのか

herdr のソケットは版 20 の `SemanticFrame` という独自の枠で話していて、
枠の形は公開されていない（`herdr-client.log` の handshake に出る）。
公開されていない枠を読み解いて繋ぐと、向こうが版を上げた日に黙って壊れる。
**`herdr` の CLI は JSON をそのまま吐く**ので、こちらを通す。

一方 tsumugi 側は `tsg --rpc` を子プロセスとして起こし、標準入出力で
JSON Lines を話す。拡張が別プロセスなのはこのためで、この台本が落ちても
端末は落ちない。
"""

import argparse
import json
import subprocess
import sys
import threading
import time

COMMAND_ID = "ext.herdr.agents"
PANE_ID = "ext.herdr.agents"
# 見に行く間隔。**短くしない** — herdr の CLI は 1 回ごとにプロセスが起きる。
POLL_SECONDS = 5


def herdr_agents():
    """herdr に訊く。答えられなければ None（走っていないのは普通のこと）。"""
    try:
        out = subprocess.run(
            ["herdr", "agent", "list"],
            capture_output=True,
            text=True,
            timeout=10,
            encoding="utf-8",
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if out.returncode != 0 or not out.stdout.strip():
        return None
    try:
        return json.loads(out.stdout)["result"]["agents"]
    except (ValueError, KeyError, TypeError):
        return None


def render(agents):
    """並べて見せる形にする。**幅は決め打ちにしない**（名前は長い）。"""
    if agents is None:
        return "herdr が走っていません。"
    if not agents:
        return "エージェントは居ません。"
    rows = [
        (
            a.get("agent_status", "?"),
            a.get("agent", "?"),
            a.get("workspace_id", ""),
            a.get("terminal_title_stripped") or a.get("terminal_title", ""),
            a.get("cwd", ""),
        )
        for a in agents
    ]
    widths = [max(len(str(r[i])) for r in rows) for i in range(3)]
    lines = []
    for status, agent, ws, title, cwd in rows:
        # 返事待ちだけ目印を付ける。**全部に付けると目印にならない。**
        mark = "●" if status == "blocked" else " "
        lines.append(
            f"{mark} {status:<{widths[0]}}  {agent:<{widths[1]}}  "
            f"{ws:<{widths[2]}}  {title}"
        )
        if cwd:
            lines.append(f"    {cwd}")
    return "\n".join(lines)


class Tsumugi:
    """`tsg --rpc` の子プロセス。1 行 1 通。"""

    def __init__(self, session):
        args = ["tsg"]
        if session:
            args += ["-s", session]
        args += ["--rpc"]
        self.proc = subprocess.Popen(
            args,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
        self.lock = threading.Lock()

    def send(self, **msg):
        with self.lock:
            self.proc.stdin.write(json.dumps(msg, ensure_ascii=False) + "\n")
            self.proc.stdin.flush()

    def lines(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except ValueError:
                continue


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-s", "--session", default=None)
    args = ap.parse_args()

    tsg = Tsumugi(args.session)
    # ペインが開いているか。**開く前に書かない** — 断られるだけで、
    # `tsg --ext-log` に意味のない ✗ が並ぶ。
    open_pane_id = {"open": False}
    tsg.send(t="ext_hello", name="herdr")
    tsg.send(
        t="register_command",
        command={
            "id": COMMAND_ID,
            "title": "herdr のエージェント",
            "title_en": "herdr agents",
            "menu": "セッション",
        },
    )
    tsg.send(t="subscribe", events=["command"])

    def open_pane():
        tsg.send(
            t="ext_pane_open",
            id=PANE_ID,
            title="herdr",
            text=render(herdr_agents()),
            dir="vertical",
        )

    def watch():
        """返事待ちになった相手だけを知らせる。**同じ相手で繰り返さない。**"""
        told = set()
        while True:
            time.sleep(POLL_SECONDS)
            agents = herdr_agents()
            if agents is None:
                continue
            blocked = {
                a.get("agent_session", {}).get("value", a.get("pane_id", ""))
                for a in agents
                if a.get("agent_status") == "blocked"
            }
            for who in blocked - told:
                name = next(
                    (
                        a.get("terminal_title_stripped") or a.get("agent", "agent")
                        for a in agents
                        if a.get("agent_session", {}).get("value") == who
                        or a.get("pane_id") == who
                    ),
                    "agent",
                )
                tsg.send(t="notify", text=f"herdr: {name} が返事待ちです", level="warn")
            told = blocked
            # 開いてあるときだけ中身を新しくする。
            if open_pane_id["open"]:
                tsg.send(t="ext_pane_write", id=PANE_ID, text=render(agents))

    threading.Thread(target=watch, daemon=True).start()

    for msg in tsg.lines():
        # 押されたら開く / 中身を新しくする。
        if msg.get("t") == "ext_pane" and msg.get("id") == PANE_ID:
            open_pane_id["open"] = True
        elif msg.get("t") == "event":
            e = msg.get("event", {})
            if e.get("e") == "command" and e.get("id") == COMMAND_ID:
                open_pane()
        # 断られた理由は黙って捨てない（`tsg --ext-log` にも残る）。
        elif msg.get("t") == "error":
            print(msg.get("message", ""), file=sys.stderr)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
