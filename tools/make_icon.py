#!/usr/bin/env python3
"""assets/icon.svg を基準に assets/icon.ico と assets/icon.png を生成する。

SVG レンダラが無くても動くよう、図形を Pillow で再現している。
大きいサイズは 16 倍スーパーサンプリング後に LANCZOS で縮小。
16/20/24px はピクセル単位で手書きする（ぼやけ防止）。
実行: python tools/make_icon.py
"""
import math
import os
from PIL import Image, ImageDraw

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
ASSETS = os.path.join(ROOT, "assets")
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SS = 16

OUTLINE = (0x5F, 0x5E, 0x5A, 255)
WHITE = (255, 255, 255, 255)
FOLD = (0xD3, 0xD1, 0xC7, 255)
GUTTER = (0xB5, 0xD4, 0xF4, 255)
TICK = (0x18, 0x5F, 0xA5, 255)
TEXT = (0x88, 0x87, 0x80, 255)


def arc(cx, cy, r, a0, a1, n=24):
    """角度(度, y下向き座標系)a0→a1 の円弧点列"""
    return [(cx + r * math.cos(math.radians(a0 + (a1 - a0) * i / n)),
             cy + r * math.sin(math.radians(a0 + (a1 - a0) * i / n))) for i in range(n + 1)]


def page_polygon(off):
    """ページ外形。中心線パスを off だけ外側(負なら内側)にオフセット。"""
    r = 2 + off
    d = off * math.sqrt(2)
    x0, x1, y0, y1 = 11 - off, 53 + off, 3 - off, 61 + off
    # 斜め辺: x - y = 37 + d
    pts = []
    pts += [(x0, y0 + r)] if False else []
    pts += arc(13, 5, r, 180, 270)  # 左上角 (x0,y0 付近)
    pts += [(37 + d + y0, y0), (x1, x1 - 37 - d)]  # 斜め辺
    pts += arc(51, 59, r, 0, 90)    # 右下
    pts += arc(13, 59, r, 90, 180)  # 左下
    return pts


def draw_master(size):
    k = size * SS / 64.0
    img = Image.new("RGBA", (size * SS, size * SS), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    def P(pts):
        return [(x * k, y * k) for x, y in pts]

    # ページ(枠 + 白)
    d.polygon(P(page_polygon(1)), fill=OUTLINE)
    d.polygon(P(page_polygon(-1)), fill=WHITE)
    # 折り返し
    fold = [(40, 3), (40, 14)] + arc(42, 14, 2, 180, 90) + [(53, 16)]
    d.polygon(P(fold), fill=FOLD)
    line = [(40, 3), (40, 14)] + arc(42, 14, 2, 180, 90) + [(53, 16)]
    d.line(P(line), fill=OUTLINE, width=int(round(2 * k)), joint="curve")
    # 折り返しの斜め辺の外側を枠色で
    d.line(P([(40, 3), (53, 16)]), fill=OUTLINE, width=int(round(2 * k)))
    # 行番号ガター
    g = [(12, 18), (21, 18), (21, 60), (13, 60)] + arc(13, 59, 1, 90, 180, 8)
    d.polygon(P(g), fill=GUTTER)

    def capsule(x0, x1, y, w, col):
        d.line(P([(x0, y), (x1, y)]), fill=col, width=int(round(w * k)))
        r = w * k / 2
        for x in (x0, x1):
            d.ellipse([x * k - r, y * k - r, x * k + r, y * k + r], fill=col)

    for y in (24, 32, 40, 48):
        capsule(15, 18, y, 2.5, TICK)
    for y, x1 in ((24, 44), (32, 48), (40, 40), (48, 46)):
        capsule(26, x1, y, 3, TEXT)
    return img.resize((size, size), Image.LANCZOS)


def draw_pixel(n):
    """16/20/24px 用のピクセル手書き版。"""
    cfg = {
        16: dict(x0=2, x1=13, f=4, g=3, tick=1, th=1, rows=[5, 8, 11], lens=[5, 4, 3]),
        20: dict(x0=2, x1=16, f=5, g=4, tick=2, th=1, rows=[7, 10, 13], lens=[6, 7, 4]),
        24: dict(x0=3, x1=20, f=6, g=5, tick=3, th=2, rows=[8, 12, 16, 20], lens=[8, 9, 6, 8]),
    }[n]
    x0, x1, f, g = cfg["x0"], cfg["x1"], cfg["f"], cfg["g"]
    y0, y1 = 0, n - 1
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    px = img.load()
    # 白地(斜め辺の内側のみ)
    for y in range(y0, y1 + 1):
        for x in range(x0, x1 + 1):
            if x - (x1 - f) > y - y0:  # 折り返しより外側(右上)は透明
                continue
            px[x, y] = WHITE
    # 折り返し塗り
    for y in range(y0, y0 + f + 1):
        for x in range(x1 - f, x1 + 1):
            a, b = x - (x1 - f), y - y0
            if 0 <= a < b and x < x1 and y < y0 + f:
                px[x, y] = FOLD
    # 外枠
    for x in range(x0, x1 - f + 1):
        px[x, y0] = OUTLINE
    for y in range(y0 + f, y1 + 1):
        px[x1, y] = OUTLINE
    for x in range(x0, x1 + 1):
        px[x, y1] = OUTLINE
    for y in range(y0, y1 + 1):
        px[x0, y] = OUTLINE
    # 折り返し: 斜め辺・縦辺・横辺
    for i in range(f + 1):
        px[x1 - f + i, y0 + i] = OUTLINE
    for y in range(y0, y0 + f + 1):
        px[x1 - f, y] = OUTLINE
    for x in range(x1 - f, x1 + 1):
        px[x, y0 + f] = OUTLINE
    # ガター
    gy0 = y0 + f
    for y in range(gy0, y1):
        for x in range(x0 + 1, x0 + g + 1):
            px[x, y] = GUTTER
    # ティック & テキスト行
    tx0 = x0 + 1 + (g - cfg["tick"]) // 2
    for row, ln in zip(cfg["rows"], cfg["lens"]):
        for h in range(cfg["th"]):
            for x in range(tx0, tx0 + cfg["tick"]):
                px[x, row + h] = TICK
            for x in range(x0 + g + 2, x0 + g + 2 + ln):
                px[x, row + h] = TEXT
    return img


def main():
    os.makedirs(ASSETS, exist_ok=True)
    images = {}
    for s in SIZES:
        images[s] = draw_pixel(s) if s in (16, 20, 24) else draw_master(s)
    # PNG(ウィンドウアイコン用)
    images[256].save(os.path.join(ASSETS, "icon.png"))
    # ICO: 大きい画像を基準に append_images で各サイズを明示的に格納
    big = images[256]
    big.save(os.path.join(ASSETS, "icon.ico"), format="ICO",
             sizes=[(s, s) for s in SIZES],
             append_images=[images[s] for s in SIZES if s != 256])
    print("wrote assets/icon.ico and assets/icon.png")


if __name__ == "__main__":
    main()
