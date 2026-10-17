#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""从图标定稿派生 Windows 需要的全部图标资产（开发期脚本，只在换图标时跑一次）。

为什么要这一步：设计稿（`icon/source/mxrun-master-1024-whitebg.png`）是**压平在白底**
上的（RGB，无 alpha）。直接拿来当 exe 图标，深色任务栏上会看到一个白色方块。
本脚本做三件事：

1. 把白底换成真正的透明：从四角 floodfill 找出与边界连通的背景区块置 alpha=0，
   再吃掉贴着背景的一圈浅色抗锯齿像素；白色字形在蓝块内部、不与背景连通，所以
   不会被误伤。
2. 归一化画布：裁到蓝块外接矩形，等比放大到画布的 96%（设计稿四周的留白是导出
   预览的边距，不是图标自身的边距），保证托盘/资源管理器里图形够大。
3. 生成各种出口格式：
     icon/mxrun-<size>.png        透明 PNG（含 1024 母版，供以后再派生）
     icon/mxrun.ico               exe 资源（Explorer / 快捷方式 / 属性），10 个尺寸
     assets/tray-<size>.rgba      托盘图标原始像素（按 DPI 档位，见 main.rs）
     assets/window-256.rgba       窗口 / 任务栏 / Alt+Tab（eframe 自己缩放）
     assets/logo-64.rgba          设置页里的品牌标（app 内 28pt 显示）

依赖：Python 3 + Pillow。用法：`python tools/derive-icon-assets.py`（工作目录无关）。
"""
from PIL import Image, ImageChops, ImageDraw, ImageFilter
import os
import struct

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MASTER = os.path.join(ROOT, 'icon', 'source', 'mxrun-master-1024-whitebg.png')
ICON_DIR = os.path.join(ROOT, 'icon')
ASSET_DIR = os.path.join(ROOT, 'assets')

PNG_SIZES = (16, 24, 32, 48, 64, 128, 256, 512, 1024)
ICO_SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
TRAY_SIZES = (16, 20, 24, 32, 48)
CANVAS = 1024
FILL_RATIO = 0.96


def cut_white_background(im: Image.Image) -> Image.Image:
    """白底 -> 透明底。返回与 `im` 同尺寸的 RGBA。"""
    w, h = im.size
    probe = im.copy()
    for seed in ((0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)):
        ImageDraw.floodfill(probe, seed, (255, 0, 0), thresh=10)

    # 被 floodfill 涂成纯红(255,0,0)的像素 = 背景。注意不能只看 R 通道：
    # 白色字形的 R 也是 255。
    r, g, b = probe.split()
    outside = ImageChops.multiply(
        r.point(lambda v: 255 if v == 255 else 0),
        g.point(lambda v: 255 if v == 0 else 0),
    )
    outside = ImageChops.multiply(outside, b.point(lambda v: 255 if v == 0 else 0))

    # 贴着背景的一圈浅色像素是设计稿与白底的抗锯齿过渡，一并去掉；
    # 蓝块自身的深色描边（luma 低）与内部白色字形（不与背景相邻）不受影响。
    light = im.convert('L').point(lambda v: 255 if v >= 200 else 0)
    catch = ImageChops.multiply(outside.filter(ImageFilter.MaxFilter(3)), light)
    holes = ImageChops.lighter(outside, catch)

    rgba = im.convert('RGBA')
    rgba.putalpha(holes.point(lambda v: 0 if v > 0 else 255))
    return rgba


def normalise(tile: Image.Image) -> Image.Image:
    """裁到图形外接矩形，等比缩放到 CANVAS 画布的 FILL_RATIO，居中。"""
    bbox = tile.getchannel('A').point(lambda v: 255 if v > 127 else 0).getbbox()
    if bbox is None:
        raise SystemExit('master has no opaque pixels — background keying went wrong')
    tile = tile.crop(bbox)
    tw, th = tile.size
    side = max(tw, th)
    inner = int(round(CANVAS * FILL_RATIO))
    scaled = tile.resize(
        (max(1, round(tw * inner / side)), max(1, round(th * inner / side))),
        Image.LANCZOS,
    )
    canvas = Image.new('RGBA', (CANVAS, CANVAS), (0, 0, 0, 0))
    canvas.paste(scaled, ((CANVAS - scaled.width) // 2, (CANVAS - scaled.height) // 2), scaled)
    return canvas


def scale(canvas: Image.Image, size: int) -> Image.Image:
    """预乘 alpha 后再缩放：否则透明像素的颜色会渗出浅色描边。"""
    return canvas.convert('RGBa').resize((size, size), Image.LANCZOS).convert('RGBA')


def write_ico(images, path: str) -> None:
    """手写 ICO：<=48px 用 BMP 条目（兼容性最好），更大的用 PNG 条目。"""
    entries = []
    for img in images:
        size = img.width
        if size >= 96:
            import io
            buf = io.BytesIO()
            img.save(buf, format='PNG')
            data = buf.getvalue()
            entries.append((size, data))
        else:
            # BITMAPINFOHEADER，高度写两倍（XOR + AND 蒙版）
            header = struct.pack('<IiiHHIIiiII', 40, size, size * 2, 1, 32, 0,
                                 size * size * 4, 0, 0, 0, 0)
            pixels = img.convert('RGBA').tobytes()
            bgra = bytearray(len(pixels))
            bgra[0::4], bgra[1::4], bgra[2::4], bgra[3::4] = (
                pixels[2::4], pixels[1::4], pixels[0::4], pixels[3::4])
            mask_row = ((size + 31) // 32) * 4
            mask = b'\x00' * (mask_row * size)   # 32bpp 下 alpha 通道说了算，蒙版全 0
            entries.append((size, header + bytes(bgra) + mask))

    count = len(entries)
    offset = 6 + 16 * count
    directory = b''
    payload = b''
    for size, data in entries:
        dim = 0 if size >= 256 else size
        directory += struct.pack('<BBBBHHII', dim, dim, 0, 0, 1, 32, len(data), offset)
        payload += data
        offset += len(data)
    with open(path, 'wb') as f:
        f.write(struct.pack('<HHH', 0, 1, count) + directory + payload)


def main() -> None:
    master = Image.open(MASTER).convert('RGB')
    if master.size != (CANVAS, CANVAS):
        master = master.resize((CANVAS, CANVAS), Image.LANCZOS)
    canvas = normalise(cut_white_background(master))

    os.makedirs(ICON_DIR, exist_ok=True)
    os.makedirs(ASSET_DIR, exist_ok=True)

    def at(size: int) -> Image.Image:
        return canvas if size == CANVAS else scale(canvas, size)

    for size in PNG_SIZES:
        at(size).save(os.path.join(ICON_DIR, f'mxrun-{size}.png'))

    write_ico([at(s) for s in ICO_SIZES], os.path.join(ICON_DIR, 'mxrun.ico'))

    def raw(img, name):
        with open(os.path.join(ASSET_DIR, name), 'wb') as f:
            f.write(img.tobytes())

    raw(scale(canvas, 256), 'window-256.rgba')
    raw(scale(canvas, 64), 'logo-64.rgba')
    for size in TRAY_SIZES:
        raw(scale(canvas, size), f'tray-{size}.rgba')

    print(f'icon/: mxrun-{{{",".join(map(str, PNG_SIZES))}}}.png + mxrun.ico '
          f'({len(ICO_SIZES)} sizes)')
    print(f'assets/: window-256.rgba, logo-64.rgba, tray-{{{",".join(map(str, TRAY_SIZES))}}}.rgba')
    print(f'ico size: {os.path.getsize(os.path.join(ICON_DIR, "mxrun.ico"))} bytes')


if __name__ == '__main__':
    main()
