"""读取应用窗口的像素分布，判断是否黑屏。

WebView2 有时会把内容画在自己的合成层里，PrintWindow 拿到的是空画面；
所以两种方式都试一次：先 PrintWindow，再用屏幕 DC 直接抓。

只做诊断，不改任何东西。
"""

import ctypes
import sys
from ctypes import wintypes

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32

PW_RENDERFULLCONTENT = 0x00000002
SRCCOPY = 0x00CC0020


class RECT(ctypes.Structure):
    _fields_ = [
        ("left", ctypes.c_long),
        ("top", ctypes.c_long),
        ("right", ctypes.c_long),
        ("bottom", ctypes.c_long),
    ]


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD),
        ("biWidth", ctypes.c_long),
        ("biHeight", ctypes.c_long),
        ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD),
        ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD),
        ("biXPelsPerMeter", ctypes.c_long),
        ("biYPelsPerMeter", ctypes.c_long),
        ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


def find_windows(keyword):
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _):
        n = user32.GetWindowTextLengthW(hwnd)
        if n > 0:
            buf = ctypes.create_unicode_buffer(n + 1)
            user32.GetWindowTextW(hwnd, buf, n + 1)
            if keyword.lower() in buf.value.lower():
                found.append((hwnd, buf.value))
        return True

    user32.EnumWindows(cb, 0)
    return found


def grab(hwnd, use_screen=False):
    r = RECT()
    if not user32.GetWindowRect(hwnd, ctypes.byref(r)):
        return None
    w = r.right - r.left
    h = r.bottom - r.top
    if w <= 0 or h <= 0:
        return None

    hdc_screen = user32.GetDC(0)
    hdc_mem = gdi32.CreateCompatibleDC(hdc_screen)
    hbmp = gdi32.CreateCompatibleBitmap(hdc_screen, w, h)
    gdi32.SelectObject(hdc_mem, hbmp)

    if use_screen:
        gdi32.BitBlt(hdc_mem, 0, 0, w, h, hdc_screen, r.left, r.top, SRCCOPY)
    else:
        user32.PrintWindow(hwnd, hdc_mem, PW_RENDERFULLCONTENT)

    bi = BITMAPINFOHEADER()
    bi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bi.biWidth = w
    bi.biHeight = -h  # 负数表示自上而下
    bi.biPlanes = 1
    bi.biBitCount = 32
    bi.biCompression = 0

    buf_len = w * h * 4
    buf = ctypes.create_string_buffer(buf_len)
    gdi32.GetDIBits(hdc_mem, hbmp, 0, h, buf, ctypes.byref(bi), 0)

    gdi32.DeleteObject(hbmp)
    gdi32.DeleteDC(hdc_mem)
    user32.ReleaseDC(0, hdc_screen)
    return w, h, buf.raw


def analyze(data):
    total = len(data) // 4
    non_dark = 0
    bright = 0
    for i in range(0, len(data), 4):
        b, g, r = data[i], data[i + 1], data[i + 2]
        if r + g + b > 30:
            non_dark += 1
        if r + g + b > 400:
            bright += 1
    return non_dark / total, bright / total


def save_png(path, w, h, bgra):
    """把 BGRA 缓冲写成 PNG（不引第三方库）。"""
    import struct
    import zlib

    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter: none
        row = bgra[y * w * 4 : (y + 1) * w * 4]
        for x in range(w):
            b, g, r = row[x * 4], row[x * 4 + 1], row[x * 4 + 2]
            raw += bytes((r, g, b, 255))

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def main():
    keyword = sys.argv[1] if len(sys.argv) > 1 else "WorkBuddy Hub"
    wins = find_windows(keyword)
    if not wins:
        print(f"没有找到标题包含 {keyword!r} 的窗口")
        return

    for hwnd, title in wins:
        print(f"窗口: {title!r}  handle={hwnd}")
        for label, use_screen in (("PrintWindow", False), ("屏幕抓取", True)):
            got = grab(hwnd, use_screen)
            if not got:
                print(f"  {label}: 抓取失败")
                continue
            w, h, data = got
            nd, br = analyze(data)
            verdict = "纯黑" if nd < 0.005 else ("有内容" if br > 0.002 else "暗但非纯黑")
            print(
                f"  {label}: {w}x{h}  非黑像素 {nd*100:.2f}%  "
                f"亮像素 {br*100:.3f}%  → {verdict}"
            )
            if use_screen:
                out = sys.argv[2] if len(sys.argv) > 2 else "window-screen.png"
            else:
                out = (sys.argv[2] if len(sys.argv) > 2 else "window.png").replace(
                    ".png", "-print.png"
                )
            save_png(out, w, h, data)
            print(f"  已保存 {out}")


if __name__ == "__main__":
    main()
