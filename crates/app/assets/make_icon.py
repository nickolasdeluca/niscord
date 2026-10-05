"""Draws Niscord's icon: a monitor with a play symbol on a blurple tile.

Writes icon.png (window icon, 256 px) and icon.ico (exe icon, 16-256 px)
next to this script. Needs Pillow:  python make_icon.py
"""

from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).parent
SCALE = 4  # draw big, then downsample for smooth edges
SIZE = 256 * SCALE
TOP = (0x6B, 0x77, 0xF5)
BOTTOM = (0x47, 0x52, 0xC4)
WHITE = (255, 255, 255, 255)


def px(fraction):
    return round(fraction * SIZE)


def draw():
    # Background: vertical gradient clipped to a rounded square.
    gradient = Image.new("RGBA", (SIZE, SIZE))
    for y in range(SIZE):
        t = y / (SIZE - 1)
        colour = tuple(round(a + (b - a) * t) for a, b in zip(TOP, BOTTOM)) + (255,)
        gradient.paste(colour, (0, y, SIZE, y + 1))
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, SIZE - 1, SIZE - 1), radius=px(0.22), fill=255)
    icon = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    icon.paste(gradient, (0, 0), mask)

    d = ImageDraw.Draw(icon)
    # Monitor outline.
    d.rounded_rectangle((px(0.17), px(0.22), px(0.83), px(0.68)), radius=px(0.06), outline=WHITE, width=px(0.065))
    # Stand.
    d.rectangle((px(0.45), px(0.68), px(0.55), px(0.77)), fill=WHITE)
    d.rounded_rectangle((px(0.32), px(0.75), px(0.68), px(0.82)), radius=px(0.035), fill=WHITE)
    # Play symbol, optically centred in the screen.
    cx, cy, r = 0.515, 0.45, 0.13
    d.polygon([(px(cx - r * 0.75), px(cy - r)), (px(cx - r * 0.75), px(cy + r)), (px(cx + r), px(cy))], fill=WHITE)
    return icon


def main():
    big = draw()
    png = big.resize((256, 256), Image.LANCZOS)
    png.save(HERE / "icon.png")
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    png.save(HERE / "icon.ico", sizes=[(s, s) for s in sizes])
    print("wrote icon.png and icon.ico")


if __name__ == "__main__":
    main()
