"""Generate spec-derived PNG inputs and RGBA expectations without an image library."""
from pathlib import Path
import struct
import zlib


def chunk(kind, data):
    return (struct.pack('>I', len(data)) + kind + data +
            struct.pack('>I', zlib.crc32(kind + data)))


def sample(x, y, depth, c):
    # Exercise 16-bit rounding, not just high-byte truncation.
    return (x * 385 + y * 601 + c * 255 + x * y * 31) % (1 << depth)


def packed(row, depth):
    if depth == 16:
        return b''.join(struct.pack('>H', v) for v in row)
    if depth == 8:
        return bytes(row)
    per = 8 // depth
    row += [0] * (-len(row) % per)
    return bytes(sum(v << (8 - depth * (i + 1)) for i, v in enumerate(row[j:j+per]))
                 for j in range(0, len(row), per))


def png(color, depth, interlaced, transparency=False):
    w, h = 9, 7
    n = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[color]
    stages = [(0, 0, 1, 1)] if not interlaced else [
        (0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4),
        (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]
    palette = [(255-i, i, (i * 39) % 256) for i in range(1 << depth)] if color == 3 else []
    raw = bytearray()
    for x0, y0, dx, dy in stages:
        for y in range(y0, h, dy):
            raw.append(0)
            raw.extend(packed([sample(x, y, depth, c) for x in range(x0, w, dx)
                               for c in range(n)], depth))
    header = struct.pack('>IIBBBBB', w, h, depth, color, 0, 0, int(interlaced))
    extra = b''
    key = [sample(1, 0, depth, c) for c in range(n)]
    if palette:
        extra += chunk(b'PLTE', bytes(v for rgb in palette for v in rgb))
    if transparency:
        trns = bytes([0, 91]) if palette else b''.join(struct.pack('>H', v) for v in key)
        extra += chunk(b'tRNS', trns)
    data = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', header) + extra + chunk(b'IDAT', zlib.compress(raw)) + chunk(b'IEND', b'')
    rgba = bytearray()
    for y in range(h):
        for x in range(w):
            vals = [sample(x, y, depth, c) for c in range(n)]
            top = (1 << depth) - 1
            scaled = [(v * 255 + top // 2) // top for v in vals]
            rgb = palette[vals[0]] if palette else ([scaled[0]] * 3 if color in (0, 4) else scaled[:3])
            alpha = scaled[-1] if color in (4, 6) else 255
            if transparency:
                alpha = [0, 91, 255][min(vals[0], 2)] if palette else (0 if vals == key else 255)
            rgba.extend([*rgb, alpha])
    return data, rgba


if __name__ == '__main__':
    path = Path(__file__).parent / 'png.tsv'
    with path.open('w') as out:
        for color, depths in [(0, (1, 2, 4, 8, 16)), (2, (8, 16)), (3, (1, 2, 4, 8)), (4, (8, 16)), (6, (8, 16))]:
            for depth in depths:
                for interlaced in (False, True):
                    for trns in ((False, True) if color in (0, 2, 3) else (False,)):
                        data, rgba = png(color, depth, interlaced, trns)
                        print(f'png-{color}-{depth}-{int(interlaced)}-{int(trns)}', 9, 7, data.hex(), rgba.hex(), sep='\t', file=out)
    signature = b'\x89PNG\r\n\x1a\n'
    bad = {'empty': b'', 'unknown': b'hello', 'short-bmp': b'BM\0\0',
           'short-gif': b'GIF89a', 'short-jpeg': b'\xff\xd8\xff'}
    for name, w, h, depth, color, payload in [
        ('zero-width', 0, 7, 8, 6, b''), ('huge', 0xffffffff, 0xffffffff, 8, 6, b''),
        ('illegal-depth', 9, 7, 3, 6, b''), ('missing-inflate', 9, 7, 8, 6, b'garbage'),
        ('short-raster', 9, 7, 8, 6, zlib.compress(bytes(36))),
        ('bad-filter', 9, 7, 8, 6, zlib.compress((b'\x07'+bytes(36))*7))]:
        bad[name] = (signature + chunk(b'IHDR', struct.pack('>IIBBBBB', w,h,depth,color,0,0,0))
                     + chunk(b'IDAT', payload) + chunk(b'IEND', b''))
    with path.with_name('invalid.tsv').open('w') as out:
        for name, data in bad.items():
            print(name, data.hex(), sep='\t', file=out)
