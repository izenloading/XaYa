#!/usr/bin/env python3
"""Export desktop icons from extra/logo/xaya-icon.png (macOS tools required)."""

import base64
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
LOGO = ROOT / "extra/logo"


def main():
    source = LOGO / "xaya-icon.png"
    for tool in ("sips", "iconutil"):
        if not shutil.which(tool):
            raise SystemExit(f"{tool} is required; run this exporter on macOS")
    if not source.is_file():
        raise SystemExit(f"Missing master artwork: {source}")

    def run(*args):
        subprocess.run(args, check=True, stdout=subprocess.DEVNULL)

    with tempfile.TemporaryDirectory(prefix="xaya-icons-") as temp:
        work = Path(temp)
        iconset = work / "xaya.iconset"
        iconset.mkdir()
        for size in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = size * scale
                suffix = "@2x" if scale == 2 else ""
                run("sips", "-z", str(pixels), str(pixels), str(source),
                    "--out", str(iconset / f"icon_{size}x{size}{suffix}.png"))
        run("iconutil", "-c", "icns", str(iconset), "-o",
            str(ROOT / "extra/osx/XaYa.app/Contents/Resources/xaya.icns"))

        # PNG-compressed ICO entries are supported on Windows Vista and later.
        entries = []
        for size in (16, 24, 32, 48, 64, 128, 256):
            output = work / f"{size}.png"
            run("sips", "-z", str(size), str(size), str(source), "--out", str(output))
            entries.append((size, output.read_bytes()))
        offset = 6 + 16 * len(entries)
        header = struct.pack("<HHH", 0, 1, len(entries))
        directory = bytearray()
        for size, data in entries:
            directory.extend(struct.pack("<BBBBHHII", size % 256, size % 256,
                                         0, 0, 1, 32, len(data), offset))
            offset += len(data)
        (ROOT / "alacritty/windows/xaya.ico").write_bytes(
            header + directory + b"".join(data for _, data in entries))

        for name in ("xaya-term.png", "xaya-term+scanlines.png"):
            run("sips", "-z", "256", "256", str(source),
                "--out", str(LOGO / "compat" / name))

    # Keep the existing Linux install/release paths, with self-contained artwork.
    encoded = base64.b64encode(source.read_bytes()).decode("ascii")
    svg = ('<svg xmlns="http://www.w3.org/2000/svg" '
           'xmlns:xlink="http://www.w3.org/1999/xlink" '
           'width="1024" height="1024" viewBox="0 0 1024 1024">\n'
           '  <title>Alacritty application icon</title>\n'
           f'  <image width="1024" height="1024" xlink:href="data:image/png;base64,{encoded}"/>\n'
           '</svg>\n')
    for directory in (LOGO, LOGO / "compat"):
        for name in ("xaya-term.svg", "xaya-simple.svg", "xaya-term+scanlines.svg"):
            (directory / name).write_text(svg)
    print("Updated macOS, Windows, Linux, and X11 icon assets.")


if __name__ == "__main__":
    main()
