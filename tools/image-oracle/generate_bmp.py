"""Tiny BMP fixtures: rows are padded to dwords; positive height is bottom-up."""
from pathlib import Path
import struct


def bmp(name, bpp, pixels, colors, rows, *, mode=0, top=False, masks=b'', core=False):
    w, h = 3, 2
    palette = bytes(v for r,g,b in colors for v in ([b,g,r] if core else [b,g,r,0]))
    if core:
        header = struct.pack('<IHHHH', 12, w, h, 1, bpp)
    else:
        header = struct.pack('<IiiHHIIiiII', 40,w,-h if top else h,1,bpp,mode,len(pixels),0,0,len(colors),0)
    offset = 14 + len(header) + len(masks) + len(palette)
    data = struct.pack('<2sIHHI', b'BM', offset+len(pixels), 0,0,offset) + header + masks + palette + pixels
    rgba = bytes(v for row in rows for i in row for v in [*rgb[i],255])
    return '\t'.join([name,str(w),str(h),data.hex(),rgba.hex()])


if __name__ == '__main__':
    rgb = [(255,0,0), (0,255,0)]
    rows = [(0,1,1),(1,0,0)]
    cases = [bmp('bmp-core-1',1, bytes.fromhex('80000000 60000000'), rgb, rows, core=True),
             bmp('bmp-top-4',4,bytes.fromhex('01100000 10000000'),rgb,rows,top=True),
             bmp('bmp-index-8',8,bytes.fromhex('01000000 00010100'),rgb,rows),
             bmp('bmp-rle8',8,bytes.fromhex('010102000000 010002010001'),rgb,rows,mode=1),
             bmp('bmp-rle4',4,bytes.fromhex('011002000000 010002110001'),rgb,rows,mode=2),
             bmp('bmp-565',16,bytes.fromhex('e00700f800f80000 00f8e007e0070000'),[],rows,mode=3,
                 masks=struct.pack('<III',0xf800,0x07e0,0x001f))]
    Path(__file__).with_name('bmp.tsv').write_text('\n'.join(cases)+'\n')
