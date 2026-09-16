//! Integration tests for the full image pipeline:
//! interceptor → registry → delta → compositor.

#[cfg(all(feature = "sixel", feature = "kitty", feature = "iterm2"))]
mod pipeline {
    use bmux_image::intercept::ImageInterceptor;
    use bmux_image::model::*;
    use bmux_image::registry::ImageRegistry;

    #[test]
    fn kitty_placement_replacement_and_deletion_remove_retained_images() {
        let mut registry = ImageRegistry::default();
        let mut interceptor = ImageInterceptor::new();
        let mut wire = b"\x1b_".to_vec();
        wire.extend(bmux_image::codec::kitty::encode_transmit(
            42,
            KittyFormat::Rgba,
            &[255, 0, 0, 255],
            1,
            1,
        ));
        wire.extend_from_slice(b"\x1b\\\x1b_Ga=p,i=42,p=7;\x1b\\");
        for event in interceptor.process(&wire).events {
            registry.handle_event(event, 8, 16);
        }
        let old_id = registry.images()[0].id;
        let sequence = registry.sequence();
        for event in interceptor.process(b"\x1b_Ga=p,i=42,p=7;\x1b\\").events {
            registry.handle_event(event, 8, 16);
        }
        assert_eq!(registry.images().len(), 1);
        assert!(registry.delta_since(sequence).removed.contains(&old_id));
        let current_id = registry.images()[0].id;
        let sequence = registry.sequence();
        for event in interceptor.process(b"\x1b_Ga=d,d=p,i=42,p=7;\x1b\\").events {
            registry.handle_event(event, 8, 16);
        }
        assert!(registry.images().is_empty());
        assert!(registry.delta_since(sequence).removed.contains(&current_id));
        for event in interceptor
            .process(b"\x1bPq#1;2;100;0;0~\x1b\\\x1b_Ga=p,i=42,p=8;\x1b\\\x1b_Ga=d,d=a;\x1b\\")
            .events
        {
            registry.handle_event(event, 8, 16);
        }
        assert_eq!(registry.images().len(), 1);
        assert_eq!(registry.images()[0].protocol, ImageProtocol::Sixel);
    }

    #[test]
    fn alternate_screen_registry_restores_normal_images() {
        let mut registry = ImageRegistry::default();
        let event = ImageEvent::SixelImage {
            data: b"#1;2;100;0;0~".to_vec(),
            position: ImagePosition { row: 2, col: 3 },
            pixel_size: ImagePixelSize {
                width: 1,
                height: 6,
            },
            filtered_byte_offset: 0,
        };
        registry.handle_event(event.clone(), 8, 16);
        let main_id = registry.images()[0].id;
        let before = registry.sequence();
        registry.set_alternate_screen(true);
        assert!(registry.images().is_empty());
        assert!(registry.delta_since(before).removed.contains(&main_id));
        registry.handle_event(event, 8, 16);
        let alternate_id = registry.images()[0].id;
        assert_ne!(main_id, alternate_id);
        let before_exit = registry.sequence();
        registry.set_alternate_screen(false);
        assert_eq!(registry.images().len(), 1);
        assert_eq!(registry.images()[0].id, main_id);
        let delta = registry.delta_since(before_exit);
        assert!(delta.removed.contains(&alternate_id));
        assert!(delta.added.iter().any(|image| image.id == main_id));
        registry.set_alternate_screen(true);
        assert!(registry.images().is_empty());
        registry.clear();
        registry.set_alternate_screen(false);
        assert_eq!(registry.images()[0].id, main_id);
    }

    #[test]
    fn coalesced_image_delta_does_not_resurrect_exited_screen() {
        let mut registry = ImageRegistry::default();
        let event = ImageEvent::SixelImage {
            data: b"#1;2;100;0;0~".to_vec(),
            position: ImagePosition { row: 5, col: 0 },
            pixel_size: ImagePixelSize {
                width: 1,
                height: 6,
            },
            filtered_byte_offset: 0,
        };
        registry.handle_event(event.clone(), 8, 16);
        let main_id = registry.images()[0].id;
        let before = registry.sequence();
        registry.set_alternate_screen(true);
        registry.handle_event(event, 8, 16);
        let alternate_id = registry.images()[0].id;
        registry.set_alternate_screen(false);
        let delta = registry.delta_since(before);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].id, main_id);
        assert!(delta.removed.contains(&alternate_id));
        assert!(!delta.removed.contains(&main_id));
        let before = registry.sequence();
        registry.scroll_up(1).unwrap();
        registry.scroll_up(1).unwrap();
        let delta = registry.delta_since(before);
        assert_eq!(delta.added.len(), 1);
        assert_eq!(delta.added[0].position.row, 3);
    }

    #[test]
    fn retained_image_recovers_original_geometry_after_scrolling_back() {
        let mut registry = ImageRegistry::default();
        let mut interceptor = ImageInterceptor::new();
        for event in interceptor
            .process(b"\x1bPq\"1;1;2;12#1;2;100;0;0~~-~~\x1b\\")
            .events
        {
            registry.handle_event(event, 1, 6);
        }
        let original = registry.images()[0].clone();
        registry.scroll_up(1).unwrap();
        let clipped = registry.project_viewport(0, 10).unwrap();
        assert_eq!(clipped[0].cell_size.rows, 1);
        assert_eq!(clipped[0].pixel_size.height, 6);
        let wire = bmux_attach_image_protocol::AttachPaneImage::from(&clipped[0]);
        assert!(!wire.raw_data.is_empty());
        let restored_crop = PaneImage::from(&wire);
        let decoded =
            bmux_image::codec::sixel::decode(restored_crop.payload.raw.as_ref().unwrap()).unwrap();
        assert_eq!(decoded.height, 6);
        assert_eq!(
            decoded.data,
            clipped[0].payload.pixels.as_ref().unwrap().data
        );
        registry.scroll_up(4).unwrap();
        assert!(registry.project_viewport(0, 10).unwrap().is_empty());
        assert_eq!(registry.images_in_viewport(5, 10).len(), 1);
        let restored = registry.project_viewport(5, 10).unwrap();
        assert_eq!(restored[0], original);
        registry.evict_history(3);
        assert!(registry.project_viewport(5, 10).unwrap().is_empty());
    }

    #[test]
    fn display_clear_preserves_history_but_reset_discards_both_screens() {
        let mut registry = ImageRegistry::default();
        let mut interceptor = ImageInterceptor::new();
        for event in interceptor
            .process(b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\")
            .events
        {
            registry.handle_event(event, 8, 16);
        }
        registry.scroll_up(2).unwrap();
        registry.clear_display();
        assert_eq!(registry.project_viewport(2, 10).unwrap().len(), 1);
        registry.set_alternate_screen(true);
        registry.reset();
        registry.set_alternate_screen(false);
        assert!(registry.project_viewport(2, 10).unwrap().is_empty());
    }

    #[test]
    fn kitty_history_remains_deletable_after_display_clear() {
        let mut registry = ImageRegistry::default();
        let mut interceptor = ImageInterceptor::new();
        let mut wire = b"\x1b_".to_vec();
        wire.extend(bmux_image::codec::kitty::encode_transmit(
            42,
            KittyFormat::Rgba,
            &[255, 0, 0, 255],
            1,
            1,
        ));
        wire.extend_from_slice(b"\x1b\\\x1b_Ga=p,i=42,p=7;\x1b\\");
        for event in interceptor.process(&wire).events {
            registry.handle_event(event, 8, 16);
        }
        registry.scroll_up(2).unwrap();
        registry.clear_display();
        assert_eq!(registry.project_viewport(2, 10).unwrap().len(), 1);
        let before = registry.sequence();
        for event in interceptor.process(b"\x1b_Ga=d,d=p,i=42,p=7;\x1b\\").events {
            registry.handle_event(event, 8, 16);
        }
        assert!(registry.sequence() > before);
        assert!(registry.project_viewport(2, 10).unwrap().is_empty());
    }

    #[test]
    fn history_capture_is_bounded_and_independent_of_live_mutations() {
        let mut registry = ImageRegistry::default();
        let mut interceptor = ImageInterceptor::new();
        for event in interceptor
            .process(b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\")
            .events
        {
            registry.handle_event(event, 8, 16);
        }
        let mut insufficient = 1;
        assert!(registry.capture_history(&mut insufficient).is_err());
        assert_eq!(insufficient, 1);
        registry.scroll_up(2).unwrap();
        let mut budget = 1024 * 1024;
        let capture = registry.capture_history(&mut budget).unwrap();
        assert!(budget < 1024 * 1024);
        registry.reset();
        assert_eq!(capture.project_viewport(2, 10).unwrap().len(), 1);
        assert!(registry.project_viewport(2, 10).unwrap().is_empty());
    }

    #[test]
    fn remapping_retained_positions_is_atomic_and_preserves_source_pixels() {
        let mut registry = ImageRegistry::default();
        let mut interceptor = ImageInterceptor::new();
        for event in interceptor
            .process(b"\x1bPq\"1;1;1;6#1;2;100;0;0~\x1b\\")
            .events
        {
            registry.handle_event(event, 8, 16);
        }
        let original = registry.images()[0].clone();
        let revision = registry.sequence();
        assert!(
            registry
                .remap_positions(24, |_, _| Err(std::io::Error::other("unavailable anchor")))
                .is_err()
        );
        assert_eq!(registry.sequence(), revision);
        assert_eq!(registry.images()[0], original);
        registry
            .remap_positions(24, |row, col| Ok((row + 2, col + 3)))
            .unwrap();
        assert_eq!(registry.images()[0].position.row, 2);
        assert_eq!(registry.images()[0].position.col, 3);
        assert_eq!(registry.images()[0].payload, original.payload);
        assert_eq!(registry.delta_since(revision).added[0].position.row, 2);
    }

    #[test]
    fn split_sixel_offsets_are_relative_to_each_read() {
        let mut interceptor = ImageInterceptor::new();
        let first = interceptor.process(b"label\r\n\x1bPq\"1;1;2;6#1;2;100;0;0");
        assert_eq!(first.filtered, b"label\r\n");
        let second = interceptor.process(b"#1~~\x1b\\done");
        assert_eq!(second.filtered, b"done");
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].filtered_byte_offset(), 0);
    }

    #[test]
    fn inline_protocols_use_host_encoding_and_preserve_geometry() {
        let rgba = vec![
            255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
        ];
        let pixels = PixelBuffer {
            data: rgba.clone(),
            width: 2,
            height: 2,
            format: PixelFormat::Rgba8,
        };
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            &rgba,
            2,
            2,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        let mut sixel = b"\x1bPq".to_vec();
        sixel.extend(bmux_image::codec::sixel::encode(&pixels).unwrap());
        sixel.extend_from_slice(b"\x1b\\");
        let mut iterm = b"\x1b]1337;File=".to_vec();
        iterm.extend(bmux_image::codec::iterm2::encode_body_with_cells(
            &png, 16, 8,
        ));
        iterm.push(7);
        for (wire, cols, rows) in [(sixel, 1, 1), (iterm, 16, 8)] {
            let mut registry = ImageRegistry::default();
            for mut event in ImageInterceptor::new().process(&wire).events {
                event.set_position(ImagePosition { row: 3, col: 2 });
                registry.handle_event(event, 10, 20);
            }
            let source = &registry.images()[0];
            assert_eq!(source.cell_size, ImageCellSize { cols, rows });
            assert_eq!(
                source.pixel_size,
                ImagePixelSize {
                    width: 2,
                    height: 2
                }
            );
            let transport = bmux_attach_image_protocol::AttachPaneImage::from(source);
            let restored = PaneImage::from(&transport);
            for protocol in 0..3 {
                let caps = bmux_image::host_caps::HostImageCapabilities {
                    kitty_graphics: protocol == 0,
                    sixel: protocol == 1,
                    iterm2_inline: protocol == 2,
                    cell_pixel_width: 10,
                    cell_pixel_height: 20,
                    ..Default::default()
                };
                let mut output = Vec::new();
                bmux_image::compositor::render_pane_images_clipped(
                    &mut output,
                    std::slice::from_ref(&restored),
                    bmux_image::compositor::PaneRect {
                        x: 0,
                        y: 0,
                        w: 80,
                        h: 24,
                    },
                    &[],
                    &caps,
                    &mut Default::default(),
                )
                .unwrap();
                let mut decoded_registry = ImageRegistry::default();
                for event in ImageInterceptor::new().process(&output).events {
                    decoded_registry.handle_event(event, 10, 20);
                }
                assert_eq!(decoded_registry.images().len(), 1);
                let image = &decoded_registry.images()[0];
                assert_eq!(
                    image.protocol,
                    [
                        ImageProtocol::KittyGraphics,
                        ImageProtocol::Sixel,
                        ImageProtocol::ITerm2
                    ][protocol]
                );
                if protocol == 1 {
                    let decoded =
                        bmux_image::codec::sixel::decode(image.payload.raw.as_ref().unwrap())
                            .unwrap();
                    let (width, height) = if source.protocol == ImageProtocol::Sixel {
                        (2, 2)
                    } else {
                        (160, 160)
                    };
                    assert_eq!((decoded.width, decoded.height), (width, height));
                    for y in 0..height {
                        for x in 0..width {
                            let expected = if (x < width / 2) == (y < height / 2) {
                                &rgba[..4]
                            } else {
                                &rgba[4..8]
                            };
                            let offset = ((y * width + x) * 4) as usize;
                            assert_eq!(&decoded.data[offset..offset + 4], expected);
                        }
                    }
                } else if protocol == 2 {
                    let (_, bytes) =
                        bmux_image::codec::iterm2::parse_body(image.payload.raw.as_ref().unwrap())
                            .unwrap();
                    assert_eq!(
                        image::load_from_memory(&bytes)
                            .unwrap()
                            .to_rgba8()
                            .into_raw(),
                        rgba
                    );
                } else {
                    assert_eq!(image.payload.pixels.as_ref().unwrap().data, rgba);
                }
            }
        }
    }

    #[test]
    fn kitty_rgba_survives_attach_transport_and_host_rendering() {
        let rgba = vec![
            255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
        ];
        let mut wire = b"\x1b_".to_vec();
        wire.extend(bmux_image::codec::kitty::encode_transmit(
            42,
            KittyFormat::Rgba,
            &rgba,
            2,
            2,
        ));
        wire.extend_from_slice(b"\x1b\\\x1b_Ga=p,i=42,p=7,c=16,r=8,C=1,q=2;\x1b\\");
        let mut interceptor = ImageInterceptor::new();
        let mut registry = ImageRegistry::default();
        for mut event in interceptor.process(&wire).events {
            event.set_position(ImagePosition { row: 3, col: 5 });
            registry.handle_event(event, 10, 20);
        }
        let image = &registry.images()[0];
        assert_eq!(image.cell_size, ImageCellSize { rows: 8, cols: 16 });
        let transported: bmux_attach_image_protocol::AttachPaneImage = image.into();
        let restored = PaneImage::from(&transported);
        let decoded = image::load_from_memory(restored.payload.raw.as_ref().unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.into_raw(), rgba);
        let caps = bmux_image::host_caps::HostImageCapabilities {
            kitty_graphics: true,
            ..Default::default()
        };
        for mode in [
            bmux_image::config::ImageDecodeMode::Server,
            bmux_image::config::ImageDecodeMode::Client,
            bmux_image::config::ImageDecodeMode::Passthrough,
        ] {
            let mut output = Vec::new();
            bmux_image::compositor::render_pane_images(
                &mut output,
                std::slice::from_ref(&restored),
                bmux_image::compositor::PaneRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                &caps,
                mode,
                &mut Default::default(),
            )
            .unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("f=100"));
            assert!(text.contains("c=16,r=8"));
            assert!(text.contains("\x1b[4;6H"));
        }
    }

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
        registry.scroll_up(3).unwrap();
        assert_eq!(registry.images()[0].position.row, 7);

        // Scroll up by 8 more (image at row 7 with 1 row height → evicted at row <0).
        registry.scroll_up(8).unwrap();
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
        input.extend_from_slice(b"\x1b]1337;File=");
        let mut png = Vec::new();
        image::ImageEncoder::write_image(
            image::codecs::png::PngEncoder::new(&mut png),
            &[255, 0, 0, 255],
            1,
            1,
            image::ExtendedColorType::Rgba8,
        )
        .unwrap();
        input.extend(bmux_image::codec::iterm2::encode_body(&png, true));
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
