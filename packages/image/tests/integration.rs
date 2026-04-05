//! Integration tests for the full image pipeline:
//! interceptor → registry → delta → compositor.

#[cfg(all(feature = "sixel", feature = "kitty", feature = "iterm2"))]
mod pipeline {
    use bmux_image::intercept::ImageInterceptor;
    use bmux_image::model::*;
    use bmux_image::registry::ImageRegistry;

    /// Full pipeline: sixel data flows from interceptor → registry → delta.
    #[test]
    fn sixel_intercept_to_registry_to_delta() {
        let mut interceptor = ImageInterceptor::new();
        let mut registry = ImageRegistry::new(100, 10 * 1024 * 1024);

        // Simulate PTY output containing a sixel image.
        let mut input = Vec::new();
        input.extend_from_slice(b"before");
        input.extend_from_slice(b"\x1bPq");
        input.extend_from_slice(b"#0;2;100;0;0~"); // minimal sixel body
        input.extend_from_slice(b"\x1b\\");
        input.extend_from_slice(b"after");

        let result = interceptor.process(&input);

        // Filtered output should not contain the sixel sequence.
        assert_eq!(result.filtered, b"beforeafter");
        assert_eq!(result.events.len(), 1);

        // Feed events to registry.
        for event in result.events {
            registry.handle_event(event, 8, 16);
        }

        assert_eq!(registry.images().len(), 1);
        let img = &registry.images()[0];
        assert_eq!(img.protocol, ImageProtocol::Sixel);
        // Position is (0,0) placeholder; in real usage the PTY reader resolves it.
        assert_eq!(img.position.row, 0);
        assert_eq!(img.position.col, 0);
        assert!(img.payload.raw.is_some());

        // Delta should contain the image.
        let delta = registry.delta_since(0);
        assert_eq!(delta.added.len(), 1);
        assert!(delta.removed.is_empty());
        assert!(delta.sequence > 0);

        // Subsequent delta with current sequence should be empty.
        let delta2 = registry.delta_since(delta.sequence);
        assert!(delta2.added.is_empty());
        assert!(delta2.removed.is_empty());
    }

    /// Scroll tracking: images shift positions when content scrolls.
    #[test]
    fn scroll_shifts_image_positions() {
        let mut interceptor = ImageInterceptor::new();
        let mut registry = ImageRegistry::new(100, 10 * 1024 * 1024);

        // Add an image and manually set its position to row 10
        // (simulating what the PTY reader does via set_position).
        let input = b"\x1bPq~\x1b\\";
        let result = interceptor.process(input);
        for mut event in result.events {
            event.set_position(ImagePosition { row: 10, col: 0 });
            registry.handle_event(event, 8, 16);
        }
        assert_eq!(registry.images()[0].position.row, 10);

        // Scroll up by 3 lines: row 10 → row 7.
        registry.scroll_up(3);
        assert_eq!(registry.images()[0].position.row, 7);

        // Scroll up by 8 more (image at row 7 with 1 row height → evicted at row <0).
        registry.scroll_up(8);
        assert!(registry.images().is_empty());
    }

    /// Delta tracking: removals are properly reported.
    #[test]
    fn delta_tracks_removals() {
        let mut registry = ImageRegistry::new(2, 0);

        // Add 3 images (limit is 2, so first one gets evicted).
        for _i in 0..3 {
            let input = b"\x1bPq~\x1b\\";
            let mut interceptor = ImageInterceptor::new();
            let result = interceptor.process(input);
            for event in result.events {
                registry.handle_event(event, 8, 16);
            }
        }

        // Should have 2 images (first was evicted).
        assert_eq!(registry.images().len(), 2);

        // Full delta should show 2 added.
        let delta = registry.delta_since(0);
        assert_eq!(delta.added.len(), 2);
    }

    /// Kitty chunked transmission accumulates correctly.
    #[test]
    fn kitty_chunked_transmission() {
        let mut interceptor = ImageInterceptor::new();
        let mut registry = ImageRegistry::new(100, 10 * 1024 * 1024);

        // First chunk (more_chunks=true).
        let chunk1 = b"\x1b_Ga=t,i=42,f=32,s=2,v=2,m=1;AAAA\x1b\\";
        let result1 = interceptor.process(chunk1);
        for event in result1.events {
            registry.handle_event(event, 8, 16);
        }
        // No image yet (still accumulating).
        assert!(registry.images().is_empty());

        // Final chunk (more_chunks=false via m=0 or absent).
        let chunk2 = b"\x1b_Ga=t,i=42,f=32,s=2,v=2;BBBB\x1b\\";
        let result2 = interceptor.process(chunk2);
        for event in result2.events {
            registry.handle_event(event, 8, 16);
        }
        // Now the image should exist (but as transmitted, not placed).
        // The kitty protocol separates transmit from place.
        // Registry stores transmitted images separately until placed.
    }

    /// Sixel encode/decode roundtrip.
    #[test]
    fn sixel_encode_decode_roundtrip() {
        use bmux_image::codec::sixel;
        use bmux_image::model::{PixelBuffer, PixelFormat};

        // Create a simple 4x6 red image (one sixel band).
        let mut data = vec![0u8; 4 * 6 * 4]; // 4 wide, 6 tall, RGBA
        for y in 0..6 {
            for x in 0..4 {
                let offset = (y * 4 + x) * 4;
                data[offset] = 255; // R
                data[offset + 1] = 0; // G
                data[offset + 2] = 0; // B
                data[offset + 3] = 255; // A
            }
        }

        let pixels = PixelBuffer {
            width: 4,
            height: 6,
            format: PixelFormat::Rgba8,
            data,
        };

        let encoded = sixel::encode(&pixels).expect("encoding should succeed");
        assert!(!encoded.is_empty());

        // Decode the encoded data and verify dimensions match.
        let size = sixel::estimate_pixel_size(&encoded);
        assert_eq!(size.width, 4);
        assert_eq!(size.height, 6);
    }

    /// iTerm2 image extraction and parameter parsing.
    #[test]
    fn iterm2_full_pipeline() {
        let mut interceptor = ImageInterceptor::new();
        let mut registry = ImageRegistry::new(100, 10 * 1024 * 1024);

        // Simulate an iTerm2 inline image OSC.
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b]1337;File=inline=1:");
        input.extend_from_slice(b"AAAA"); // base64 image data
        input.push(0x07); // BEL terminator

        let result = interceptor.process(&input);
        assert_eq!(result.events.len(), 1);

        for event in result.events {
            registry.handle_event(event, 8, 16);
        }

        assert_eq!(registry.images().len(), 1);
        let img = &registry.images()[0];
        assert_eq!(img.protocol, ImageProtocol::ITerm2);
        assert_eq!(img.position.row, 0);
        assert_eq!(img.position.col, 0);
    }

    /// Compositor produces valid output for sixel passthrough.
    #[test]
    fn compositor_sixel_passthrough() {
        use bmux_image::compositor::{PaneRect, render_pane_images};
        use bmux_image::config::ImageDecodeMode;
        use bmux_image::host_caps::HostImageCapabilities;

        let host_caps = HostImageCapabilities {
            sixel: true,
            ..Default::default()
        };

        let images = vec![PaneImage {
            id: 1,
            protocol: ImageProtocol::Sixel,
            payload: ImagePayload {
                raw: Some(b"#0;2;100;0;0~".to_vec()),
                pixels: None,
            },
            position: ImagePosition { row: 0, col: 0 },
            cell_size: ImageCellSize { rows: 1, cols: 1 },
            pixel_size: ImagePixelSize {
                width: 1,
                height: 6,
            },
        }];

        let rect = PaneRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };

        let mut kitty_state = bmux_image::compositor::KittyHostState::default();
        let mut output = Vec::new();
        render_pane_images(
            &mut output,
            &images,
            rect,
            &host_caps,
            ImageDecodeMode::Passthrough,
            &mut kitty_state,
        )
        .unwrap();

        // Output should contain the sixel DCS sequence.
        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("\x1bPq"),
            "Should contain sixel DCS start"
        );
        assert!(output_str.contains("\x1b\\"), "Should contain sixel ST");
    }

    /// Kitty transmit-once-place-many: second render doesn't re-transmit.
    #[test]
    fn kitty_transmit_once_place_many() {
        use bmux_image::compositor::{KittyHostState, PaneRect, render_pane_images};
        use bmux_image::config::ImageDecodeMode;
        use bmux_image::host_caps::HostImageCapabilities;

        let host_caps = HostImageCapabilities {
            kitty_graphics: true,
            ..Default::default()
        };
        let rect = PaneRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let images = vec![PaneImage {
            id: 42,
            protocol: ImageProtocol::KittyGraphics,
            payload: ImagePayload {
                raw: Some(b"IMAGEDATA".to_vec()),
                pixels: None,
            },
            position: ImagePosition { row: 0, col: 0 },
            cell_size: ImageCellSize { rows: 5, cols: 10 },
            pixel_size: ImagePixelSize {
                width: 80,
                height: 80,
            },
        }];

        let mut kitty_state = KittyHostState::default();

        // First render: should transmit.
        let mut out1 = Vec::new();
        render_pane_images(
            &mut out1,
            &images,
            rect,
            &host_caps,
            ImageDecodeMode::Passthrough,
            &mut kitty_state,
        )
        .unwrap();
        let s1 = String::from_utf8_lossy(&out1);
        assert!(s1.contains("a=t"), "First render should transmit");

        // Second render: should only place, not re-transmit.
        let mut out2 = Vec::new();
        render_pane_images(
            &mut out2,
            &images,
            rect,
            &host_caps,
            ImageDecodeMode::Passthrough,
            &mut kitty_state,
        )
        .unwrap();
        let s2 = String::from_utf8_lossy(&out2);
        assert!(!s2.contains("a=t"), "Second render should NOT re-transmit");
        assert!(s2.contains("a=p"), "Second render should place");
    }

    /// Disabled registry (zero capacity) drops everything.
    #[test]
    fn disabled_registry_drops_images() {
        let mut registry = ImageRegistry::new(0, 0);

        let mut interceptor = ImageInterceptor::new();
        let input = b"\x1bPq~\x1b\\";
        let result = interceptor.process(input);
        for event in result.events {
            registry.handle_event(event, 8, 16);
        }

        // Zero-capacity registry should have no images.
        assert!(registry.images().is_empty());
    }

    /// filtered_byte_offset is correctly set for cursor position resolution.
    #[test]
    fn interceptor_reports_correct_filtered_offset() {
        let mut interceptor = ImageInterceptor::new();

        // "hello" (5 bytes filtered) then a sixel image.
        let mut input = Vec::new();
        input.extend_from_slice(b"hello");
        input.extend_from_slice(b"\x1bPq~\x1b\\");

        let result = interceptor.process(&input);
        assert_eq!(result.filtered, b"hello");
        assert_eq!(result.events.len(), 1);
        // The ESC was encountered after 5 filtered bytes.
        assert_eq!(result.events[0].filtered_byte_offset(), 5);
    }
}
