# Images & Compression

bmux supports inline terminal images through three protocols: **Sixel**, **Kitty Graphics**, and **iTerm2 inline images**. Images are automatically intercepted, stored in a per-pane registry, and re-emitted to the host terminal. They also appear in GIF recordings.

## Supported Image Protocols

| Protocol | Origin | How It Works |
|----------|--------|-------------|
| **Sixel** | DEC, 1980s | ASCII-encoded raster data in DCS escape sequences. Widely supported by modern terminals (foot, WezTerm, mlterm, contour, xterm with `-ti vt340`). |
| **Kitty Graphics** | kitty terminal | Binary pixel data or PNG in APC escape sequences. Supports transparency, animation, and efficient placement. Used by kitty, WezTerm, Ghostty. |
| **iTerm2** | iTerm2 | Base64-encoded image files (PNG, JPEG, GIF) in OSC sequences. Used by iTerm2, WezTerm, mintty. |

## How It Works

When a program running inside a bmux pane emits image escape sequences, bmux:

1. **Intercepts** the escape sequences in the PTY output stream
2. **Stores** the image data in a per-pane image registry (with position, dimensions, and raw protocol bytes)
3. **Re-emits** the image to the host terminal using the appropriate protocol for display
4. **Tracks changes** -- when images scroll, move, or are removed, the registry updates automatically

This is transparent to programs. Any application that displays images in a terminal will work inside bmux without modification.

## Testing Image Support

### Using kitten (Kitty protocol)

If you have kitty or the `kitten` CLI installed:

```bash
kitten icat /path/to/image.png
```

### Using timg (all protocols)

`timg` supports all three protocols with flag switches:

```bash
brew install timg

# Kitty protocol
timg -pk /path/to/image.png

# Sixel protocol
timg -ps /path/to/image.png

# iTerm2 protocol
timg -pi /path/to/image.png
```

### Raw escape sequence test (no dependencies)

You can test each protocol with raw escape sequences. This sends a tiny 1x1 red pixel:

**Kitty:**
```bash
printf '\e_Gf=32,s=1,v=1,a=T;/w8AAP8=\e\\'
```

**Sixel (small red block):**
```bash
printf '\ePq"1;1;1;1#0;2;100;0;0#0!6~-!6~-!6~-!6~-!6~-!6~\e\\'
```

## GIF Export with Images

Images automatically appear in exported GIF recordings. When you record a session that displays images, bmux captures structured image data alongside the terminal text. During GIF export, images are decoded to pixels and composited onto the rasterized text frame.

To record and export:

```bash
# Start a recording
bmux recording start

# Display some images in your session...

# Stop and export
bmux recording stop
bmux recording list              # find your recording ID
bmux recording export --format gif <recording-id>
```

Supported decode formats in GIF export:
- **Sixel**: Full native decode to RGBA pixels
- **Kitty**: Raw RGB/RGBA pixels and PNG-compressed payloads
- **iTerm2**: PNG, JPEG, GIF, and BMP via the `image` crate

## Compression

bmux compresses image data and remote connections to reduce bandwidth and improve performance. Compression is enabled by default and works transparently.

### Configuration

```toml
[behavior.compression]
# Master switch. Set to false to disable all compression.
enabled = true

# Image payload compression algorithm.
# Compresses Sixel, Kitty, and iTerm2 image data before IPC transport.
# Typical reduction: 5-15x for sixel text, 3-20x for raw pixel data.
# Pre-compressed formats (kitty PNG) are automatically skipped.
# Options: auto, none, zstd, lz4
images = "auto"

# Remote connection compression.
# Wraps TLS gateway and Iroh P2P connections in streaming compression.
# Local Unix socket connections are never compressed.
# Options: auto, none, zstd
remote = "auto"

# Compression level for zstd (1-19, ignored for lz4).
# 1 = fastest, 3 = good balance (default), 9+ = diminishing returns.
level = 3
```

### Disabling Compression

```toml
# Disable all compression
[behavior.compression]
enabled = false
```

```toml
# Disable only image compression
[behavior.compression]
images = "none"
```

```toml
# Disable only remote connection compression
# (useful when SSH already compresses the tunnel)
[behavior.compression]
remote = "none"
```

### Remote Compression Matching

Both the client and the server gateway must have the same `remote` setting. If one side compresses and the other doesn't, the connection will fail. The default `auto` setting works correctly when both sides use the same bmux version and config.

## Image Configuration

```toml
[behavior.images]
# Master switch for image protocol support.
enabled = true

# How image decoding is distributed.
# passthrough = forward raw bytes (fastest, default)
# server = decode on server, re-encode for client
# client = send raw bytes, client decodes
decode_mode = "passthrough"

# Maximum image payload size (bytes). Images larger than this are discarded.
max_image_bytes = 10485760  # 10 MiB

# Maximum images per pane. Oldest images are evicted when exceeded.
max_images_per_pane = 100
```

## Troubleshooting

### Images not appearing

1. Verify your host terminal supports the image protocol being used. Run the raw escape sequence tests above directly in your terminal (outside bmux) to confirm.
2. Check that `behavior.images.enabled` is `true` (default).
3. Ensure the image is within size limits (`max_image_bytes`).

### Images appearing in wrong positions after split/zoom

This is expected during layout transitions. Images are re-fetched from the server with correct positions on the next render cycle.

### Performance with many large images

- Reduce `max_images_per_pane` to limit memory usage per pane.
- Compression is on by default and significantly reduces IPC bandwidth for image-heavy sessions.
- Consider `behavior.compression.level = 1` for faster compression if CPU is the bottleneck.

### Remote connections failing with compression

If TLS or Iroh connections fail immediately after connecting, the most likely cause is a compression mismatch. Ensure both the client and server gateway have the same `behavior.compression.remote` setting. Set both to `"none"` to diagnose.
