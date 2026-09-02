import sys, zlib, struct

def read_ppm(path):
    d = open(path, 'rb').read()
    # P6 header: magic, width, height, maxval; whitespace/comment separated
    pos = 0
    fields = []
    while len(fields) < 4:
        while d[pos:pos+1].isspace():
            pos += 1
        if d[pos:pos+1] == b'#':
            while d[pos:pos+1] not in (b'\n', b'\r'):
                pos += 1
            continue
        start = pos
        while not d[pos:pos+1].isspace():
            pos += 1
        fields.append(d[start:pos])
    pos += 1
    magic, w, h, mx = fields[0], int(fields[1]), int(fields[2]), int(fields[3])
    assert magic == b'P6' and mx == 255, (magic, mx)
    return w, h, d[pos:pos + w*h*3]

def write_png(path, w, h, rgb):
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        raw += rgb[y*w*3:(y+1)*w*3]
    def chunk(tag, data):
        c = struct.pack('>I', len(data)) + tag + data
        return c + struct.pack('>I', zlib.crc32(tag + data) & 0xffffffff)
    png = b'\x89PNG\r\n\x1a\n'
    png += chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
    png += chunk(b'IDAT', zlib.compress(bytes(raw), 6))
    png += chunk(b'IEND', b'')
    open(path, 'wb').write(png)

w, h, rgb = read_ppm(sys.argv[1])
print(f'{w}x{h}')
write_png(sys.argv[2], w, h, rgb)
