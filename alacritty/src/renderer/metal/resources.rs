//! Immutable resources shared by all windows on the winit main thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice, MTLRenderPipelineState};

use super::{Error, create_pipeline};

thread_local! {
    // All callers require a MainThreadMarker, so this is one process-wide cache
    // in the window thread. Cache successes only; initialization errors can retry.
    static SHARED: RefCell<Option<Rc<SharedResources>>> = const { RefCell::new(None) };
}

pub(super) struct SharedResources {
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
    pub text: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    pub rect: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    pub build_us: f64,
}

impl SharedResources {
    pub fn get(_main_thread: MainThreadMarker) -> Result<(Rc<Self>, bool), Error> {
        SHARED.with(|slot| {
            if let Some(resources) = slot.borrow().as_ref() {
                return Ok((Rc::clone(resources), true));
            }
            // Do not hold a RefCell borrow across framework calls.
            let resources = Rc::new(Self::new()?);
            *slot.borrow_mut() = Some(Rc::clone(&resources));
            Ok((resources, false))
        })
    }

    fn new() -> Result<Self, Error> {
        let device = MTLCreateSystemDefaultDevice()
            .ok_or_else(|| Error::Other("no Metal device available".into()))?;
        let started = Instant::now();
        let library = device
            .newLibraryWithSource_options_error(
                &NSString::from_str(include_str!("shaders.metal")),
                None,
            )
            .map_err(|err| Error::Other(format!("Metal shader compilation failed: {err}")))?;
        let text = create_pipeline(&device, &library, "terminal_text", true)?;
        let rect = create_pipeline(&device, &library, "terminal_rect", false)?;
        Ok(Self { device, text, rect, build_us: started.elapsed().as_secs_f64() * 1e6 })
    }
}
