"""tsumugi のアイコンを作る。

見た目を手で描いたバイナリで持たず、**作り方をコードで持つ**。
色を変えたくなったときに、元データを探しに行かなくて済む。

    python assets/icon.py

出力:
    assets/icon.rgba    256x256 の生 RGBA。ウィンドウ / タスクバーの絵
                        （実行時に読むので PNG デコーダを持ち込まない）
    assets/icon.png     README や配布物に貼る用
    assets/tsumugi.ico  ショートカットと Explorer 用（16〜256 の多重解像度）

意匠 —— 「紡ぎ」。

    プロンプトの `>` を、**1 本の平たい紐を折り返した形**として描く。
    名前が紡ぐことなら印もそう在るべきで、ただの折れ線にはしない。
    紐であることは、縁の光・折り返しの陰・落ちる影で言う。

    背景には斜めの織り目を敷く。**16px では消える濃さ**にしてあり、
    大きく出したときだけ効く。小さいときに効かせようとすると、
    肝心の山形が濁る。

    右下の青はカーソル。「ここに文字が入る」という一点だけを言う。

**輪郭は触らない。** 立体に見せる細工は全部シルエットの内側に入れてある。
外形をいじる案（2 本の紐を交差させ、頂点から先を行き過ぎさせる）は実際に
試して捨てた。32px で、組んだ紐ではなく**折れた棒**に見える。
「まず `>` に読めること」より先に、凝りたい欲を出さない。

**16px で崩れないことが最優先。** 大きい絵をそのまま縮めると細部が泥に
なるので、要素は 3 つ（山形・カーソル・板）に絞り、線は太く保つ。
織り目・縁の光・影は、消えたところで意味が変わらないものだけにしてある。
"""

import os

from PIL import Image, ImageChops, ImageDraw, ImageFilter

HERE = os.path.dirname(os.path.abspath(__file__))
S = 1024  # 大きく描いて縮める（角の階段を消す）

# 板。tsumugi の既定テーマ「夜霧」の背景に合わせてある。
BG_TOP = (24, 29, 39, 255)
BG_BOTTOM = (11, 14, 19, 255)
EDGE = (44, 53, 68, 255)
# 上端の光。板を「面」に見せるためだけのもの。
SHEEN = (255, 255, 255, 26)
# 織り目。**この薄さが要点**で、濃くすると 16px で山形が濁る。
WEAVE = (255, 255, 255, 10)

# 紐。上が明るく下が深い金。単色にすると板に貼った紙に見える。
AMBER_LIT = (247, 209, 138, 255)
AMBER_DIM = (188, 138, 62, 255)
# 縁の光と陰。**紐の内側にだけ**入れる（輪郭は動かさない）。
AMBER_EDGE = (255, 236, 196, 190)
AMBER_UNDER = (120, 84, 30, 150)
# 折り返しで落ちる陰と、板へ落ちる影。
FOLD = (0, 0, 0, 52)
DROP = (0, 0, 0, 110)

BLUE = (122, 180, 235, 255)
BLUE_GLOW = (86, 150, 220, 70)


def rounded_mask(size, radius):
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, size - 1, size - 1], radius=radius, fill=255)
    return m


def linear_gradient(size, top, bottom):
    """縦のグラデーションを 1 枚返す。"""
    g = Image.new("RGBA", (1, size))
    for y in range(size):
        t = y / (size - 1)
        g.putpixel((0, y), tuple(int(top[i] + (bottom[i] - top[i]) * t) for i in range(4)))
    return g.resize((size, size))


def diagonal_gradient(size, lit, dim, n=64):
    """左上から右下への斜めグラデーション。紐の丸みはこれで出す。

    小さく作って伸ばす。1024x1024 を 1 画素ずつ埋めると目に見えて遅く、
    なだらかな面なので粗く作っても差が出ない。
    """
    g = Image.new("RGBA", (n, n))
    px = g.load()
    for y in range(n):
        for x in range(n):
            t = (x + y) / (2 * (n - 1))
            px[x, y] = tuple(int(lit[i] + (dim[i] - lit[i]) * t) for i in range(4))
    return g.resize((size, size), Image.BILINEAR)


def stroke_mask(size, segments, width):
    """線分の並びを太い丸端の帯として描いたマスク。

    端と継ぎ目に円を置くのは、角が尖ると 16px で潰れて欠けるから。
    """
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    points = []
    for a, b in segments:
        d.line([a, b], fill=255, width=width)
        points += [a, b]
    for p in points:
        d.ellipse([p[0] - width / 2, p[1] - width / 2, p[0] + width / 2, p[1] + width / 2], fill=255)
    return m


def paint(base, mask, fill):
    """マスクの形に `fill`（画像か色）を塗る。

    **色のアルファはマスクへ掛ける。** `paste` はマスクだけを不透明度に使い、
    色側のアルファを見ないので、薄く置いたつもりのものが真っ白に乗る
    （実際にそれで織り目が板を塗り潰した）。
    """
    if isinstance(fill, Image.Image):
        base.paste(fill, (0, 0), mask)
        return
    alpha = fill[3] if len(fill) > 3 else 255
    if alpha != 255:
        mask = mask.point(lambda v, a=alpha: v * a // 255)
    base.paste(Image.new("RGBA", base.size, fill[:3] + (255,)), (0, 0), mask)


def weave_texture(size):
    """斜めの織り目。両方向に等間隔で通す。"""
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    step = int(size * 0.075)
    w = max(1, int(size * 0.010))
    for i in range(-size, size * 2, step):
        d.line([(i, 0), (i + size, size)], fill=255, width=w)
        d.line([(i, size), (i + size, 0)], fill=255, width=w)
    return m


def build(size=S):
    art = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    panel = rounded_mask(size, int(size * 0.22))

    # 板
    art.paste(linear_gradient(size, BG_TOP, BG_BOTTOM), (0, 0), panel)

    # 織り目（板の内側にだけ）
    texture = ImageChops.multiply(weave_texture(size), panel)
    paint(art, texture, WEAVE)

    # 上端の光。上ほど強く、板の 4 割で消える。板を「面」に見せるためだけのもの。
    sheen = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(sheen)
    top = int(size * 0.40)
    for y in range(top):
        d.line([(0, y), (size, y)], fill=int(255 * (1 - y / top) ** 2))
    paint(art, ImageChops.multiply(sheen, panel), SHEEN)

    # 縁。明るい背景の上に置いたときに輪郭が消えないように。
    ImageDraw.Draw(art).rounded_rectangle(
        [size * 0.012, size * 0.012, size * 0.988, size * 0.988],
        radius=int(size * 0.212),
        outline=EDGE,
        width=int(size * 0.018),
    )

    # ---- 山形（`>`）------------------------------------------------------
    #
    # 折り返した平たい紐。**外形は素直な `>` のまま**にして、
    # 紐であることは内側（縁の光・折り返しの陰・落ちる影）だけで言う。
    w = int(size * 0.104)
    top_end = (size * 0.268, size * 0.246)
    bot_end = (size * 0.268, size * 0.754)
    vertex = (size * 0.652, size * 0.500)
    arms = [(top_end, vertex), (vertex, bot_end)]
    body = stroke_mask(size, arms, w)

    # 板へ落ちる影。紐が板の上に在ることを言うのはこれだけ。
    drop = body.filter(ImageFilter.GaussianBlur(size * 0.018))
    drop = ImageChops.offset(drop, int(size * 0.010), int(size * 0.014))
    paint(art, ImageChops.multiply(drop, panel), DROP)

    paint(art, body, diagonal_gradient(size, AMBER_LIT, AMBER_DIM))

    # 縁。**輪郭を動かさずに**厚みを出すため、内側だけを削って光と陰を置く。
    shift = max(1, int(w * 0.13))
    lit_edge = ImageChops.subtract(body, ImageChops.offset(body, shift, shift))
    dim_edge = ImageChops.subtract(body, ImageChops.offset(body, -shift, -shift))
    paint(art, ImageChops.multiply(lit_edge, body), AMBER_EDGE)
    paint(art, ImageChops.multiply(dim_edge, body), AMBER_UNDER)

    # 折り返しの陰。頂点だけを少し沈ませる。
    fold = Image.new("L", (size, size), 0)
    ImageDraw.Draw(fold).ellipse(
        [vertex[0] - w * 0.52, vertex[1] - w * 0.52, vertex[0] + w * 0.52, vertex[1] + w * 0.52],
        fill=255,
    )
    fold = fold.filter(ImageFilter.GaussianBlur(size * 0.014))
    paint(art, ImageChops.multiply(fold, body), FOLD)

    # ---- カーソル ----------------------------------------------------------
    cw = int(size * 0.086)
    y = size * 0.742
    bar = [((size * 0.700, y), (size * 0.812, y))]

    glow = stroke_mask(size, bar, int(cw * 1.7)).filter(ImageFilter.GaussianBlur(size * 0.020))
    paint(art, ImageChops.multiply(glow, panel), BLUE_GLOW)
    paint(art, stroke_mask(size, bar, cw), BLUE)

    return art


def main():
    art = build()

    png = art.resize((256, 256), Image.LANCZOS)
    png.save(os.path.join(HERE, "icon.png"))

    # 生 RGBA。winit にそのまま渡せる形で持つと、実行時に画像デコーダが要らない。
    with open(os.path.join(HERE, "icon.rgba"), "wb") as f:
        f.write(png.convert("RGBA").tobytes())

    # 小さい方は個別に縮める。1 枚から一気に落とすより輪郭が残る。
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    frames = [art.resize((s, s), Image.LANCZOS) for s in sizes]
    frames[-1].save(
        os.path.join(HERE, "tsumugi.ico"),
        format="ICO",
        sizes=[(s, s) for s in sizes],
        append_images=frames[:-1],
    )
    print("wrote icon.png / icon.rgba / tsumugi.ico")


if __name__ == "__main__":
    main()
