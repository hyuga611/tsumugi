"""撮った素材から紹介動画を組む。

素材は 1 場面につき数枚の静止画。そのままつなぐと**ただのスライド**に
なるので、次の 3 つを掛ける。

- **寄る**（Ken Burns）。見せたい一点へゆっくり寄る。動画映えの大半はこれ。
- **溜める**。変化のある場面は長く、つなぎは短く。
- **言葉を置く**。字幕は 1 場面 1 文。読み切れる長さにする。

出力は 1280x800 / 30fps の MP4。X・note は MP4、README と DEV/Zenn は
そこから作る GIF を使う。

    python tools/make_video.py ja
    python tools/make_video.py en
"""

import os
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
FFMPEG = os.path.expandvars(
    r"%LOCALAPPDATA%\Microsoft\WinGet\Links\ffmpeg.exe"
)
FPS = 30
W, H = 1280, 800

# 場面。(素材の名前, 秒数, 字幕, 寄り先) 寄り先は (x, y, 倍率) を 0〜1 で。
# 倍率 1.0 は等倍。None なら動かさない。
SCENES_JA = [
    ("open", 6.0, "ターミナルの画面を、そのまま読んで・選んで・編集できる", None),
    ("typed", 5.0, "打てば動く。まずは普通のターミナル", None),
    ("failed", 7.0, "左のふちに、コマンドの成否が出る（緑は成功・赤は失敗）", (0.02, 0.55, 2.2)),
    ("search", 8.0, "/ で探す。打つたびに飛び、一致が光る", (0.06, 0.52, 1.9)),
    ("hints", 12.0, "Space l で画面のパスにラベル。1 キーで開く", (0.04, 0.25, 2.0)),
    ("folded", 11.0, "Space O で出力を畳む。何を畳んだかが残る", (0.10, 0.08, 2.2)),
    ("render", 11.0, "Space m で Markdown を読む形に", None),
    ("raw", 6.0, "もう一度で素のまま。構文強調つきで編集できる", None),
    ("diff", 9.0, "Space g で git diff を色付きで開く", None),
    ("difffold", 9.0, "ファイル単位に畳める", (0.10, 0.08, 2.0)),
    ("blocked", 14.0, "AI エージェントが返事待ちになると、ここが光る", (0.30, 0.96, 2.4)),
    ("jumped", 8.0, "Space a でそこへ飛ぶ。探さなくていい", None),
    ("help", 10.0, "F1。マウスだけで使えるところから書いてある", None),
    ("light", 7.0, "配色は 3 つ。設定は保存した瞬間に効く", None),
    ("back", 6.0, "tsumugi — github.com/hyuga611/tsumugi", None),
]

SCENES_EN = [
    ("open", 6.0, "The terminal screen is a document. Read it, select it, edit it.", None),
    ("typed", 5.0, "It is a normal terminal first.", None),
    ("failed", 7.0, "The left edge marks every command's exit code.", (0.02, 0.55, 2.2)),
    ("search", 8.0, "Press / and it jumps as you type.", (0.06, 0.52, 1.9)),
    ("hints", 12.0, "Space l labels every path. One key opens it.", (0.04, 0.25, 2.0)),
    ("folded", 11.0, "Space O folds output — and says what it hid.", (0.10, 0.08, 2.2)),
    ("render", 11.0, "Space m renders Markdown in place.", None),
    ("raw", 6.0, "Press again for the source, with highlighting.", None),
    ("diff", 9.0, "Space g opens git diff in colour.", None),
    ("difffold", 9.0, "Fold it one file at a time.", (0.10, 0.08, 2.0)),
    ("blocked", 14.0, "When an agent needs you, this lights up.", (0.30, 0.96, 2.4)),
    ("jumped", 8.0, "Space a jumps there. No hunting.", None),
    ("help", 10.0, "F1 starts with what the mouse alone can do.", None),
    ("light", 7.0, "Three themes. Config applies the moment you save.", None),
    ("back", 6.0, "tsumugi — github.com/hyuga611/tsumugi", None),
]


def font(size: int) -> ImageFont.FreeTypeFont:
    """字幕の字。日本語が出る字体を優先する。"""
    for name in ("YuGothM.ttc", "meiryo.ttc", "msgothic.ttc", "segoeui.ttf"):
        p = Path(os.environ["WINDIR"]) / "Fonts" / name
        if p.exists():
            try:
                return ImageFont.truetype(str(p), size)
            except OSError:
                continue
    return ImageFont.load_default()


def caption(img: Image.Image, text: str) -> Image.Image:
    """下に帯を敷いて 1 文だけ置く。**画面を隠さない高さ**に留める。"""
    out = img.convert("RGB")
    d = ImageDraw.Draw(out, "RGBA")
    f = font(30)
    pad, bar = 28, 84
    d.rectangle([0, H - bar, W, H], fill=(12, 14, 20, 235))
    d.rectangle([0, H - bar, W, H - bar + 3], fill=(224, 165, 74, 255))
    d.text((pad, H - bar + 24), text, font=f, fill=(232, 236, 244))
    return out


def frames_for(src: Image.Image, seconds: float, zoom):
    """1 場面ぶんのコマ。`zoom` があれば、そこへゆっくり寄る。"""
    n = max(1, int(seconds * FPS))
    for i in range(n):
        if zoom is None:
            yield src
            continue
        t = i / max(1, n - 1)
        # 最初はゆっくり、後半で寄る（ease-in-out）
        e = t * t * (3 - 2 * t)
        zx, zy, zmax = zoom
        scale = 1.0 + (zmax - 1.0) * e
        cw, ch = W / scale, H / scale
        cx, cy = zx * W, zy * H
        left = min(max(cx - cw / 2, 0), W - cw)
        top = min(max(cy - ch / 2, 0), H - ch)
        yield src.crop((left, top, left + cw, top + ch)).resize((W, H), Image.LANCZOS)


def build(lang: str) -> Path:
    shots = ROOT / "target" / "v" / f"shots-{lang}"
    scenes = SCENES_JA if lang == "ja" else SCENES_EN
    work = ROOT / "target" / "v" / f"frames-{lang}"
    if work.exists():
        for f in work.iterdir():
            f.unlink()
    work.mkdir(parents=True, exist_ok=True)

    n = 0
    for name, seconds, text, zoom in scenes:
        # その場面の最後の 1 枚（動きが終わった絵）を使う
        candidates = sorted(shots.glob(f"*_{name}.png"))
        if not candidates:
            print(f"  素材が無い: {name}")
            continue
        src = Image.open(candidates[-1]).convert("RGB").resize((W, H), Image.LANCZOS)
        for frame in frames_for(src, seconds, zoom):
            n += 1
            caption(frame, text).save(work / f"{n:05d}.png")
        print(f"  {name}: {seconds}s")

    out = ROOT / "target" / "v" / f"tsumugi-{lang}.mp4"
    subprocess.run(
        [
            FFMPEG, "-y", "-loglevel", "error",
            "-framerate", str(FPS),
            "-i", str(work / "%05d.png"),
            "-c:v", "libx264", "-pix_fmt", "yuv420p",
            "-preset", "slow", "-crf", "20",
            str(out),
        ],
        check=True,
    )
    return out


if __name__ == "__main__":
    lang = sys.argv[1] if len(sys.argv) > 1 else "ja"
    print(f"[{lang}]")
    path = build(lang)
    print(f"できました: {path}  ({path.stat().st_size / 1024 / 1024:.1f} MB)")
