//! The Dock icon's badge.

use log::debug;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;

/// Show `count` on the Dock icon, or no badge for zero.
pub fn set_badge(mtm: MainThreadMarker, count: usize) {
    debug!("Dock badge: {count}");
    if count > 0 && super::notification::is_available() {
        super::notification::request_badge_permission();
    }
    let label = (count > 0).then(|| NSString::from_str(&count.to_string()));
    NSApplication::sharedApplication(mtm).dockTile().setBadgeLabel(label.as_deref());
}
