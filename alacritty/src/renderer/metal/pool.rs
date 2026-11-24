//! Per-window reusable buffers. Only completed commands return resources here.

use std::collections::VecDeque;
use std::mem::size_of_val;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus, MTLDevice, MTLResourceOptions,
};

use super::{Error, Instance, MAX_FRAMES_IN_FLIGHT, Upload};

const MIN_CAPACITY: usize = 4096;
const MAX_CACHED_BUFFER: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct FrameBuffers {
    pub instances: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    pub uploads: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    pub upload_offsets: Vec<usize>,
}

#[derive(Default)]
pub(super) struct AllocationStats {
    pub instance_allocations: u64,
    pub upload_allocations: u64,
    pub reuses: u64,
    pub instance_bytes: usize,
    pub upload_bytes: usize,
}

impl FrameBuffers {
    pub fn prepare(
        &mut self,
        device: &ProtocolObject<dyn MTLDevice>,
        instances: &[Instance],
        uploads: &[Upload],
    ) -> Result<AllocationStats, Error> {
        let mut stats = AllocationStats {
            instance_bytes: size_of_val(instances),
            ..AllocationStats::default()
        };
        self.upload_offsets.clear();
        for upload in uploads {
            let offset = stats
                .upload_bytes
                .checked_add(255)
                .map(|value| value & !255)
                .ok_or_else(|| Error::Other("Metal upload offset overflow".into()))?;
            stats.upload_bytes = offset
                .checked_add(upload.bytes.len())
                .ok_or_else(|| Error::Other("Metal upload size overflow".into()))?;
            self.upload_offsets.push(offset);
        }
        if stats.instance_bytes != 0 {
            if ensure_buffer(&mut self.instances, device, stats.instance_bytes)? {
                stats.instance_allocations += 1;
            } else {
                stats.reuses += 1;
            }
        }
        if stats.upload_bytes != 0 {
            if ensure_buffer(&mut self.uploads, device, stats.upload_bytes)? {
                stats.upload_allocations += 1;
            } else {
                stats.reuses += 1;
            }
        }
        // All writes happen before encoding/submission, and this frame slot was
        // acquired exclusively from the completed/free list.
        if let Some(buffer) = &self.instances {
            copy_into(buffer, 0, instances);
        }
        if let Some(buffer) = &self.uploads {
            for (upload, &offset) in uploads.iter().zip(&self.upload_offsets) {
                copy_into(buffer, offset, upload.bytes.as_slice());
            }
        }
        Ok(stats)
    }

    fn trim(&mut self) {
        for buffer in [&mut self.instances, &mut self.uploads] {
            if buffer.as_ref().is_some_and(|buffer| buffer.length() > MAX_CACHED_BUFFER) {
                *buffer = None;
            }
        }
        self.upload_offsets.clear();
    }

    fn bytes(&self) -> usize {
        self.instances.as_ref().map_or(0, |buffer| buffer.length())
            + self.uploads.as_ref().map_or(0, |buffer| buffer.length())
    }
}

struct InFlight {
    command: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    buffers: FrameBuffers,
}

#[derive(Default)]
pub(super) struct FramePool {
    in_flight: VecDeque<InFlight>,
    available: Vec<FrameBuffers>,
}

impl FramePool {
    pub fn is_full(&self) -> bool {
        self.in_flight.len() >= MAX_FRAMES_IN_FLIGHT
    }

    pub fn acquire(&mut self) -> Option<FrameBuffers> {
        if self.in_flight.len() >= MAX_FRAMES_IN_FLIGHT {
            return None;
        }
        Some(self.available.pop().unwrap_or_default())
    }

    pub fn submit(
        &mut self,
        command: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        buffers: FrameBuffers,
    ) {
        command.commit();
        self.in_flight.push_back(InFlight { command, buffers });
    }

    pub fn reap(
        &mut self,
        mut completed: impl FnMut(&ProtocolObject<dyn MTLCommandBuffer>),
    ) -> Result<(), Error> {
        while let Some(frame) = self.in_flight.front() {
            match frame.command.status() {
                MTLCommandBufferStatus::Completed => {
                    completed(&frame.command);
                    if let Some(mut frame) = self.in_flight.pop_front() {
                        frame.buffers.trim();
                        if self.available.len() < MAX_FRAMES_IN_FLIGHT {
                            self.available.push(frame.buffers);
                        }
                    }
                },
                MTLCommandBufferStatus::Error => {
                    let error = format!("Metal command buffer failed: {:?}", frame.command.error());
                    // Failed resources are never recycled. Other queued commands
                    // stay retained until finish/teardown, including on this path.
                    self.in_flight.pop_front();
                    return Err(Error::Other(error));
                },
                _ => break,
            }
        }
        Ok(())
    }

    pub fn finish(&self) {
        if let Some(frame) = self.in_flight.back() {
            frame.command.waitUntilCompleted();
        }
    }

    pub fn cached_bytes(&self) -> usize {
        self.available.iter().map(FrameBuffers::bytes).sum()
    }
}

fn ensure_buffer(
    slot: &mut Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    device: &ProtocolObject<dyn MTLDevice>,
    required: usize,
) -> Result<bool, Error> {
    if slot.as_ref().is_some_and(|buffer| buffer.length() >= required) {
        return Ok(false);
    }
    let capacity = capacity_for(required)
        .ok_or_else(|| Error::Other("Metal buffer capacity overflow".into()))?;
    let buffer = device
        .newBufferWithLength_options(capacity, MTLResourceOptions::StorageModeShared)
        .ok_or_else(|| {
            Error::Other(format!("Metal buffer allocation failed ({capacity} bytes)"))
        })?;
    *slot = Some(buffer);
    Ok(true)
}

fn copy_into<T>(buffer: &ProtocolObject<dyn MTLBuffer>, offset: usize, data: &[T]) {
    let length = size_of_val(data);
    assert!(offset <= buffer.length() && length <= buffer.length() - offset);
    // SAFETY: this private helper is used only for initialized bytes and the
    // padding-free repr(C) Instance layout. The exclusively free shared buffer
    // is large enough, does not alias the CPU slice, and no GPU work reads it
    // until FramePool::submit. No writable pointer escapes this copy.
    unsafe {
        std::ptr::copy_nonoverlapping(
            data.as_ptr().cast::<u8>(),
            buffer.contents().as_ptr().cast::<u8>().add(offset),
            length,
        );
    }
}

fn capacity_for(required: usize) -> Option<usize> {
    required.max(MIN_CAPACITY).checked_next_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_metal::*;

    #[test]
    fn geometric_growth_is_bounded_and_overflow_is_rejected() {
        assert_eq!(capacity_for(10), Some(4096));
        assert_eq!(capacity_for(4097), Some(8192));
        assert_eq!(capacity_for(usize::MAX), None);
    }

    #[test]
    #[ignore = "requires a Metal-capable Mac"]
    fn gpu_reuses_completed_buffers_and_coalesces_uploads() {
        objc2::rc::autoreleasepool(|_| {
            let device = objc2_metal::MTLCreateSystemDefaultDevice().unwrap();
            let queue = device.newCommandQueue().unwrap();
            let mut pool = FramePool::default();
            let instances =
                [Instance { rect: [0.; 4], uv: [0.; 4], color: [1.; 4], params: [0.; 4] }];
            let descriptor = MTLTextureDescriptor::new();
            descriptor.setPixelFormat(MTLPixelFormat::RGBA8Unorm);
            descriptor.setStorageMode(MTLStorageMode::Private);
            // SAFETY: a 1x1 texture is supported on all Metal devices.
            unsafe {
                descriptor.setWidth(1);
                descriptor.setHeight(1);
            }
            let texture = device.newTextureWithDescriptor(&descriptor).unwrap();
            let uploads = [11u8, 22].map(|value| Upload {
                texture: texture.clone(),
                bytes: vec![value; 256],
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                width: 1,
                height: 1,
                stride: 256,
            });
            let mut first = pool.acquire().unwrap();
            let stats = first.prepare(&device, &instances, &uploads).unwrap();
            assert_eq!(stats.instance_allocations, 1);
            assert_eq!(stats.upload_allocations, 1);
            assert_eq!(first.upload_offsets, [0, 256]);
            assert_eq!(stats.upload_bytes, 512);
            let readback = device
                .newBufferWithLength_options(512, MTLResourceOptions::StorageModeShared)
                .unwrap();
            let original = first.instances.as_ref().unwrap().contents();
            let command = queue.commandBuffer().unwrap();
            // Uncommitted resources must remain ineligible for reuse.
            let held = queue.commandBuffer().unwrap();
            pool.in_flight.push_back(InFlight { command: held, buffers: FrameBuffers::default() });
            pool.reap(|_| panic!("uncommitted frame was reclaimed")).unwrap();
            assert!(pool.available.is_empty());
            pool.in_flight.pop_front();
            let blit = command.blitCommandEncoder().unwrap();
            // SAFETY: both buffers contain at least 512 bytes; the frame's
            // coalesced upload storage is initialized over this entire region.
            unsafe {
                blit.copyFromBuffer_sourceOffset_toBuffer_destinationOffset_size(
                    first.uploads.as_ref().unwrap(),
                    0,
                    &readback,
                    0,
                    512,
                );
            }
            blit.endEncoding();
            pool.submit(command, first);
            pool.finish();
            pool.reap(|_| ()).unwrap();
            // SAFETY: the GPU copy completed and readback owns 512 initialized bytes.
            let bytes = unsafe {
                std::slice::from_raw_parts(readback.contents().as_ptr().cast::<u8>(), 512)
            };
            assert!(bytes[..256].iter().all(|&value| value == 11));
            assert!(bytes[256..].iter().all(|&value| value == 22));
            let mut second = pool.acquire().unwrap();
            let stats = second.prepare(&device, &instances, &uploads).unwrap();
            assert_eq!(stats.instance_allocations, 0);
            assert_eq!(stats.upload_allocations, 0);
            assert_eq!(stats.reuses, 2);
            assert_eq!(second.instances.as_ref().unwrap().contents(), original);
            // Large temporary buffers are discarded once GPU work completes,
            // rather than increasing the per-window retained pool indefinitely.
            second.uploads = device.newBufferWithLength_options(
                MAX_CACHED_BUFFER + 1,
                MTLResourceOptions::StorageModeShared,
            );
            assert!(second.uploads.is_some());
            pool.submit(queue.commandBuffer().unwrap(), second);
            pool.finish();
            pool.reap(|_| ()).unwrap();
            assert!(pool.acquire().unwrap().uploads.is_none());
        });
    }
}
