"""tsumugi のアイコンを作る。

見た目を手で描いたバイナリで持たず、**作り方をコードで持つ**。
色を変えたくなったときに、元データを探しに行かなくて済む。

    python assets/icon.py

出力:
    assets/icon.rgba    256x256 の生 RGBA。ウィンドウ / タスクバーの絵
                        （実行時に読むので PNG デコーダを持ち込まない）
    assets/icon.png     README や配布物に貼る用
    assets/tsumugi.ico  ショートカットと Explorer 用（16〜256 の多重解像度）

意匠: 暗い角丸の板に、琥珀のシェブロン（プロンプト）と青いカーソル。
16px でも形が崩れないことを最優先にして、線は太く・要素は 2 つだけにした。
"""

import os

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
S = 1024  # 大きく描いて縮める（角の階段を消す）

BG_TOP = (20, 24, 33, 255)
BG_BOTTOM = (12, 15, 21, 255)
EDGE = (38, 46, 60, 255)
AMBER = (226, 182, 103, 255)
BLUE = (112, 168, 220, 255)


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=255)
    return m


def vertical_gradient(size, top, bottom):
    g = Image.new("RGBA", (1, size))
    for y in range(size):
        t = y / (size - 1)
        g.putpixel(
            (0, y),
            tuple(int(top[i] + (bottom[i] - top[i]) * t) for i in range(4)),
        )
    return g.resize((size, size))


def build(size=S):
    tile = vertical_gradient(size, BG_TOP, BG_BOTTOM)
    tile.putalpha(rounded_mask(size, int(size * 0.22)))

    d = ImageDraw.Draw(tile)

    # 縁。明るい背景の上に置いたときに輪郭が消えないように。
    d.rounded_rectangle(
        [size * 0.012, size * 0.012, size * 0.988, size * 0.988],
        radius=int(size * 0.212),
        outline=EDGE,
        width=int(size * 0.018),
    )

    # プロンプトのシェブロン。丸い端で線を継ぐ（角が尖ると 16px で潰れる）。
    w = int(size * 0.105)
    a = (size * 0.325, size * 0.285)
    b = (size * 0.585, size * 0.500)
    c = (size * 0.325, size * 0.715)
    d.line([a, b], fill=AMBER, width=w)
    d.line([b, c], fill=AMBER, width=w)
    for p in (a, b, c):
        d.ellipse(
            [p[0] - w / 2, p[1] - w / 2, p[0] + w / 2, p[1] + w / 2],
            fill=AMBER,
        )

    # カーソル。ここに文字が入る、という一点だけを言う。
    cw = int(size * 0.088)
    y = size * 0.715
    d.line([(size * 0.640, y), (size * 0.760, y)], fill=BLUE, width=cw)
    for x in (size * 0.640, size * 0.760):
        d.ellipse(
            [x - cw / 2, y - cw / 2, x + cw / 2, y + cw / 2],
            fill=BLUE,
        )

    return tile


def main():
    art = build()

    png = art.resize((256, 256), Image.LANCZOS)
    png.save(os.path.join(HERE, "icon.png"))

    # 生 RGBA。winit にそのまま渡せる形で持つと、実行時に画像デコーダが要らない。
    with open(os.path.join(HERE, "icon.rgba"), "wb") as f:
        f.write(png.convert("RGBA").tobytes())

    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    png.save(
        os.path.join(HERE, "tsumugi.ico"),
        format="ICO",
        sizes=[(s, s) for s in sizes],
    )
    print("wrote icon.png / icon.rgba / tsumugi.ico")


if __name__ == "__main__":
    main()
