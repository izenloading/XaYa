use super::*;

#[test]
fn atlas_wraps_rows_without_overlapping_or_advancing_on_failure() {
    let mut shelf = Shelf::default();
    assert_eq!(shelf.insert(6, 3, 8), Some((0, 0)));
    assert_eq!(shelf.insert(3, 4, 8), Some((0, 3)));
    assert_eq!(shelf.insert(6, 2, 8), None);
    assert_eq!(shelf.insert(5, 1, 8), Some((3, 3)));
    assert_eq!(shelf.insert(8, 1, 8), Some((0, 7)));
    assert_eq!(shelf.insert(1, 1, 8), None);
}

#[test]
fn upload_preserves_subpixel_coverage_and_row_alignment() {
    let (bytes, stride) = upload_bytes(&BitmapBuffer::Rgb(vec![1, 2, 3, 4, 5, 6]), 1, 2).unwrap();
    assert_eq!(stride, 256);
    assert_eq!(&bytes[..4], &[1, 2, 3, 255]);
    assert_eq!(&bytes[256..260], &[4, 5, 6, 255]);
    assert!(bytes[4..256].iter().all(|&byte| byte == 0));
}

#[test]
fn upload_preserves_premultiplied_emoji_and_rejects_invalid_lengths() {
    let (bytes, _) = upload_bytes(&BitmapBuffer::Rgba(vec![30, 20, 10, 40]), 1, 1).unwrap();
    assert_eq!(&bytes[..4], &[30, 20, 10, 40]);
    assert!(upload_bytes(&BitmapBuffer::Rgb(vec![1, 2]), 1, 1).is_none());
}

#[test]
fn shader_buffer_layouts_are_stable() {
    assert_eq!(std::mem::size_of::<Instance>(), 64);
    assert_eq!(std::mem::size_of::<Uniforms>(), 48);
}

/// Compile the actual MSL and read pixels rendered by the actual GPU. This is
/// intentionally opt-in so ordinary tests also work on headless CI machines.
#[test]
#[ignore = "requires a Metal-capable Mac; run with --ignored --nocapture"]
fn gpu_renders_subpixel_masks_emoji_and_premultiplied_backgrounds() {
    autoreleasepool(|_| {
        let device = MTLCreateSystemDefaultDevice().expect("Metal device");
        let queue = device.newCommandQueue().unwrap();
        let library = device
            .newLibraryWithSource_options_error(
                &NSString::from_str(include_str!("shaders.metal")),
                None,
            )
            .expect("compile production Metal shaders");
        let pipeline = create_pipeline(&device, &library, "terminal_text", true).unwrap();
        // Compile decoration pipeline too, even though this test draws text.
        create_pipeline(&device, &library, "terminal_rect", false).unwrap();
        let descriptor = MTLTextureDescriptor::new();
        descriptor.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        descriptor.setUsage(MTLTextureUsage::RenderTarget);
        descriptor.setStorageMode(MTLStorageMode::Private);
        // SAFETY: 8x8 is within every Metal device's texture dimensions.
        unsafe {
            descriptor.setWidth(8);
            descriptor.setHeight(8);
        }
        let target = device.newTextureWithDescriptor(&descriptor).unwrap();
        descriptor.setPixelFormat(MTLPixelFormat::RGBA8Unorm);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(MTLStorageMode::Shared);
        // SAFETY: 2x1 is within every Metal device's texture dimensions.
        unsafe {
            descriptor.setWidth(2);
            descriptor.setHeight(1);
        }
        let atlas = device.newTextureWithDescriptor(&descriptor).unwrap();
        // First texel is RGB subpixel coverage; second is premultiplied emoji.
        let texels = [255u8, 128, 0, 255, 32, 16, 8, 128];
        // SAFETY: the initialized eight-byte slice covers a 2x1 RGBA region.
        // This test hasn't submitted any GPU reads of the texture yet.
        unsafe {
            atlas.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion {
                    origin: MTLOrigin { x: 0, y: 0, z: 0 },
                    size: MTLSize { width: 2, height: 1, depth: 1 },
                },
                0,
                NonNull::from(&texels).cast(),
                8,
            );
        }
        let instances = [
            // Top-left: white text through the RGB mask onto black.
            Instance {
                rect: [0., 0., 4., 4.],
                uv: [0.25, 0.5, 0., 0.],
                color: [1.; 4],
                params: [1., 0., 0., 0.],
            },
            // Top-right: emoji onto black.
            Instance {
                rect: [4., 0., 4., 4.],
                uv: [0.75, 0.5, 0., 0.],
                color: [1.; 4],
                params: [2., 0., 0., 0.],
            },
            // Bottom-left: half-transparent red background, replacing black.
            Instance {
                rect: [0., 4., 4., 4.],
                uv: [0.; 4],
                color: [1., 0., 0., 0.5],
                params: [0.; 4],
            },
        ];
        let instances = buffer(&device, &instances).unwrap();
        let uniforms =
            Uniforms { size: [8., 8., 0., 0.], cell: [4., 4., 0., 1.], decoration: [0.; 4] };
        let readback = device
            .newBufferWithLength_options(256 * 8, MTLResourceOptions::StorageModeShared)
            .unwrap();
        let command = queue.commandBuffer().unwrap();
        let pass = MTLRenderPassDescriptor::new();
        // SAFETY: attachment zero exists on every render pass descriptor.
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor { red: 0., green: 0., blue: 0., alpha: 1. });
        let encoder = command.renderCommandEncoderWithDescriptor(&pass).unwrap();
        encoder.setRenderPipelineState(&pipeline);
        // SAFETY: production shader layouts match these initialized repr(C)
        // values; the three instances and fixed six vertices are in bounds.
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(&instances), 0, 0);
            encoder.setVertexBytes_length_atIndex(
                NonNull::from(&uniforms).cast(),
                size_of_val(&uniforms),
                1,
            );
            encoder.setFragmentTexture_atIndex(Some(&atlas), 0);
            encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                MTLPrimitiveType::Triangle,
                0,
                6,
                3,
            );
        }
        encoder.endEncoding();
        let blit = command.blitCommandEncoder().unwrap();
        // SAFETY: an 8x8 RGBA texture fits the 256-byte row stride and buffer.
        unsafe {
            blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                &target, 0, 0, MTLOrigin { x: 0, y: 0, z: 0 }, MTLSize { width: 8, height: 8, depth: 1 },
                &readback, 0, 256, 256 * 8,
            );
        }
        blit.endEncoding();
        command.commit();
        command.waitUntilCompleted();
        assert_eq!(command.status(), MTLCommandBufferStatus::Completed, "{:?}", command.error());
        // SAFETY: GPU completion guarantees the initialized readback bytes are
        // visible, and this slice stays within the retained buffer allocation.
        let pixels = unsafe {
            std::slice::from_raw_parts(readback.contents().as_ptr().cast::<u8>(), 256 * 8)
        };
        let pixel = |x: usize, y: usize| &pixels[y * 256 + x * 4..y * 256 + x * 4 + 4];
        // BGRA8 storage. Allow one code value for fixed-point blend rounding.
        for (actual, expected) in [
            (pixel(1, 1), [0, 128, 255, 255]),
            (pixel(5, 1), [8, 16, 32, 255]),
            (pixel(1, 5), [0, 0, 128, 128]),
            (pixel(5, 5), [0, 0, 0, 255]),
        ] {
            assert!(
                actual.iter().zip(expected).all(|(&a, b)| a.abs_diff(b) <= 1),
                "actual {actual:?}, expected {expected:?}"
            );
        }
        println!(
            "GPU verified on {}: RGB coverage, emoji, premultiplied background, untouched clear",
            device.name()
        );
    });
}
