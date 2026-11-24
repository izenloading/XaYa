use objc2::runtime::AnyObject;
use objc2_foundation::{NSDictionary, NSProcessInfo, NSString, NSUserDefaults, ns_string};

use crate::config::window::DEFAULT_NAME;

pub mod dock;
pub mod locale;
pub mod notification;
pub mod proc;

/// Name the process like the app, rather than after its lowercase executable.
///
/// winit builds the application menu's "About", "Hide" and "Quit" items from the process name,
/// so this has to run before the event loop starts.
pub fn set_process_name() {
    NSProcessInfo::processInfo().setProcessName(&NSString::from_str(DEFAULT_NAME));
}

pub fn disable_autofill() {
    unsafe {
        NSUserDefaults::standardUserDefaults().registerDefaults(
            &NSDictionary::<NSString, AnyObject>::from_slices(
                &[ns_string!("NSAutoFillHeuristicControllerEnabled")],
                &[ns_string!("NO")],
            ),
        );
    }
}
