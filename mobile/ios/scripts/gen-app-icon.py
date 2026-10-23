#!/usr/bin/env python3
"""生成 iOS App 图标（1024x1024 PNG，无第三方依赖）。

用途：仓库不提交巨大二进制图标时，可用本脚本重新生成：
    python mobile/ios/scripts/gen-app-icon.py
输出：mobile/ios/DriveSync/Resources/Assets.xcassets/AppIcon.appiconset/AppIcon-1024.png

图形：深蓝→墨绿渐变底 + 白色圆角剪贴板轮廓 + 绿色对勾（表示同步完成）。
纯 Python 手写 PNG（zlib + struct），不需要 Pillow。
"""

from __future__ import annotations

import math
import os
import struct
import zlib

SIZE = 1024
OUTPUT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..",
    "DriveSync",
    "Resources",
    "Assets.xcassets",
    "AppIcon.appiconset",
    "AppIcon-1024.png",
)


def lerp(a: float, b: float, t: float) -> float:
    return a + (b - a) * t


def rounded_rect_distance(x: float, y: float, cx: float, cy: float, half_w: float, half_h: float, radius: float) -> float:
    """点到圆角矩形边界的距离（内部为负）。"""
    qx = abs(x - cx) - (half_w - radius)
    qy = abs(y - cy) - (half_h - radius)
    outside = math.hypot(max(qx, 0.0), max(qy, 0.0))
    inside = min(max(qx, qy), 0.0)
    return outside + inside - radius


def segment_distance(px: float, py: float, ax: float, ay: float, bx: float, by: float) -> float:
    abx = bx - ax
    aby = by - ay
    apx = px - ax
    apy = py - ay
    denom = abx * abx + aby * aby
    t = 0.0 if denom == 0 else max(0.0, min(1.0, (apx * abx + apy * aby) / denom))
    proj_x = ax + abx * t
    proj_y = ay + aby * t
    return math.hypot(px - proj_x, py - proj_y)


def blend(dst: tuple[float, float, float], src: tuple[float, float, float], alpha: float) -> tuple[float, float, float]:
    return (
        lerp(dst[0], src[0], alpha),
        lerp(dst[1], src[1], alpha),
        lerp(dst[2], src[2], alpha),
    )


def build_pixels() -> bytearray:
    # 背景：左上深蓝 (#0B1F33) → 右下墨绿 (#12694F)，加一点径向高光。
    top_left = (11, 31, 51)
    bottom_right = (18, 105, 79)
    board = (240, 255, 249)  # 剪贴板
    accent = (52, 235, 168)  # 对勾

    raw = bytearray()
    cx = SIZE / 2.0
    for y in range(SIZE):
        raw.append(0)  # PNG filter type: none
        for x in range(SIZE):
            t = (x + y) / (2.0 * SIZE)
            r = lerp(top_left[0], bottom_right[0], t)
            g = lerp(top_left[1], bottom_right[1], t)
            b = lerp(top_left[2], bottom_right[2], t)

            # 柔和径向高光
            radial = max(0.0, 1.0 - math.hypot(x - cx, y - SIZE * 0.42) / (SIZE * 0.62))
            r, g, b = blend((r, g, b), (30, 120, 96), radial * 0.35)

            # 剪贴板外轮廓（白色圆角描边）
            d_board = rounded_rect_distance(x, y, cx, SIZE * 0.54, SIZE * 0.27, SIZE * 0.32, SIZE * 0.075)
            if abs(d_board) <= SIZE * 0.028:
                alpha = 1.0 - abs(abs(d_board) - SIZE * 0.0) / (SIZE * 0.028)
                r, g, b = blend((r, g, b), board, min(1.0, max(0.0, alpha)))

            # 剪贴板顶部夹子
            d_clip = rounded_rect_distance(x, y, cx, SIZE * 0.245, SIZE * 0.105, SIZE * 0.045, SIZE * 0.028)
            if d_clip <= 0:
                r, g, b = blend((r, g, b), board, 1.0)

            # 绿色对勾
            thickness = SIZE * 0.042
            d_check = min(
                segment_distance(x, y, SIZE * 0.385, SIZE * 0.555, SIZE * 0.475, SIZE * 0.645),
                segment_distance(x, y, SIZE * 0.475, SIZE * 0.645, SIZE * 0.640, SIZE * 0.455),
            )
            if d_check <= thickness:
                alpha = 1.0 - max(0.0, (d_check - thickness * 0.55)) / (thickness * 0.45)
                r, g, b = blend((r, g, b), accent, min(1.0, max(0.0, alpha)))

            raw.extend((int(r), int(g), int(b), 255))
    return raw


def write_png(path: str, raw: bytearray) -> None:
    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)  # 8bit RGBA
    idat = zlib.compress(bytes(raw), 9)
    with open(path, "wb") as handle:
        handle.write(sig)
        handle.write(chunk(b"IHDR", ihdr))
        handle.write(chunk(b"IDAT", idat))
        handle.write(chunk(b"IEND", b""))


def main() -> None:
    raw = build_pixels()
    write_png(OUTPUT, raw)
    print(f"wrote {os.path.normpath(OUTPUT)} ({os.path.getsize(OUTPUT)} bytes)")


if __name__ == "__main__":
    main()
