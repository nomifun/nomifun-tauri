from PIL import Image
import numpy as np

for name in ["Snipaste_2026-07-06_11-01-40.png", "Snipaste_2026-07-06_14-09-27.png"]:
    p = f"C:/Users/rika0/Pictures/{name}"
    im = Image.open(p).convert("RGB")
    a = np.asarray(im)
    h, w, _ = a.shape
    r, g, b = a[:, :, 0].astype(int), a[:, :, 1].astype(int), a[:, :, 2].astype(int)
    red = (r > 200) & (g < 80) & (b < 90) & (abs(r - 237) < 45) & (abs(g - 28) < 55) & (abs(b - 36) < 60)
    ys, xs = np.where(red)
    print(f"\n=== {name}  size={w}x{h} red_px={int(red.sum())}")
    if red.sum():
        print(f"  red bbox: x[{xs.min()}..{xs.max()}] y[{ys.min()}..{ys.max()}]")
        colcount = red.sum(axis=0)
        rowcount = red.sum(axis=1)
        span_y = ys.max() - ys.min()
        span_x = xs.max() - xs.min()
        vcols = [i for i, c in enumerate(colcount) if c > span_y * 0.4]
        hrows = [i for i, c in enumerate(rowcount) if c > span_x * 0.4]
        print(f"  strong vertical red columns x: {vcols}")
        print(f"  strong horizontal red rows y: {hrows}")
        # isolated red dots (resize handles): find red clusters that are small
        # crude: red columns with modest counts
        modest = [(i, int(c)) for i, c in enumerate(colcount) if 3 < c < span_y * 0.4]
        print(f"  #modest red columns (dots/short marks): {len(modest)}")
