// Independent AppKit producer/consumer. No named system board is selectable.
#import <AppKit/AppKit.h>

static BOOL image(NSPasteboard *board, BOOL write, NSPasteboardType type) {
    const unsigned char pixels[] = {255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,0,255};
    if (write) {
        NSBitmapImageRep *rep = [[NSBitmapImageRep alloc] initWithBitmapDataPlanes:NULL
            pixelsWide:2 pixelsHigh:2 bitsPerSample:8 samplesPerPixel:4 hasAlpha:YES
            isPlanar:NO colorSpaceName:NSDeviceRGBColorSpace bytesPerRow:8 bitsPerPixel:32];
        if (!rep.bitmapData) return NO;
        memcpy(rep.bitmapData, pixels, sizeof(pixels));
        NSData *data = [rep representationUsingType:[type isEqual:NSPasteboardTypePNG] ?
            NSBitmapImageFileTypePNG : NSBitmapImageFileTypeTIFF properties:@{}];
        return data && [board setData:data forType:type];
    }
    NSData *data = [board dataForType:type];
    NSBitmapImageRep *rep = data ? [NSBitmapImageRep imageRepWithData:data] : nil;
    if (rep.pixelsWide != 2 || rep.pixelsHigh != 2) return NO;
    if (rep.samplesPerPixel < 3 || rep.samplesPerPixel > 4 || rep.bitsPerSample != 8) return NO;
    for (int i = 0; i < 4; ++i) {
        NSUInteger rgba[4] = {0, 0, 0, 255};
        [rep getPixel:rgba atX:i%2 y:i/2];
        for (int c = 0; c < 4; ++c) {
            if (rgba[c] != pixels[4*i+c]) {
                fprintf(stderr, "pixel %d channel %d: expected %u, got %lu\n", i, c, pixels[4*i+c], rgba[c]);
                return NO;
            }
        }
    }
    return YES;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc != 3 || strlen(argv[1]) != 32 || strspn(argv[1], "0123456789abcdef") != 32) return 2;
        NSString *name = [@"org.bmux.clipboard.test." stringByAppendingString:@(argv[1])];
        NSPasteboard *board = [NSPasteboard pasteboardWithName:name];
        if (!board || ![board.name isEqual:name]) return 3;
        NSString *op = @(argv[2]);
        if ([op isEqual:@"release"]) { [board releaseGlobally]; return 0; }
        BOOL write = [op hasPrefix:@"write-"];
        if (write && [board clearContents] < 0) return 4;
        NSString *text = @"bmux 🦀\n東京";
        if ([op isEqual:@"write-text"]) return ![board setString:text forType:NSPasteboardTypeString];
        if ([op isEqual:@"read-text"]) return ![[board stringForType:NSPasteboardTypeString] isEqual:text];
        if ([op isEqual:@"write-png"] || [op isEqual:@"read-png"]) return !image(board, write, NSPasteboardTypePNG);
        if ([op isEqual:@"write-tiff"] || [op isEqual:@"read-tiff"]) return !image(board, write, NSPasteboardTypeTIFF);
        return 2;
    }
}
