//! Native macOS notifications through `UNUserNotificationCenter`.
//!
//! Posting from xaya's own process makes macOS show the app bundle's icon and lists xaya under
//! System Settings → Notifications. `osascript` would attribute them to Script Editor instead.
//!
//! Each notification remembers the window that posted it, and clicking it brings that window
//! (or tab) to the front.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use block2::RcBlock;
use log::{debug, warn};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{
    NSBundle, NSDictionary, NSError, NSNumber, NSProcessInfo, NSString, ns_string,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

use crate::event::{Event, EventType};

/// Permissions asked for: banners, their sound, and the Dock badge.
const AUTHORIZATION: UNAuthorizationOptions = UNAuthorizationOptions::Alert
    .union(UNAuthorizationOptions::Sound)
    .union(UNAuthorizationOptions::Badge);

/// Where clicks on notifications are sent, set by [`init`].
static PROXY: OnceLock<Mutex<EventLoopProxy<Event>>> = OnceLock::new();

/// Get ready to show notifications and to route clicks on them back to their windows.
///
/// Called at startup, so the delegate is in place before macOS delivers any click.
pub fn init(mtm: MainThreadMarker, proxy: EventLoopProxy<Event>) {
    if !is_available() {
        return;
    }
    let _ = PROXY.set(Mutex::new(proxy));
    install_delegate(mtm, &UNUserNotificationCenter::currentNotificationCenter());
}

/// Whether native notifications can be used.
///
/// `UNUserNotificationCenter` requires a bundle identifier and raises an exception without one,
/// so a bare binary (e.g. `cargo run`) has to fall back to `osascript`.
pub fn is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| NSBundle::mainBundle().bundleIdentifier().is_some())
}

/// Post a notification for `window`. Must only be called when [`is_available`] is `true`.
pub fn show(mtm: MainThreadMarker, title: &str, body: &str, window: WindowId) {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    install_delegate(mtm, &center);

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let window = NSNumber::numberWithUnsignedLongLong(u64::from(window));
    // `userInfo` remembers the posting window, see `focus_posting_window`.
    let user_info =
        NSDictionary::<NSString, NSNumber>::from_slices(&[ns_string!("xayaWindow")], &[&*window]);
    // SAFETY: An `NSDictionary` of strings to numbers is a valid property-list `userInfo`; the
    // untyped parameter only erases its generics.
    unsafe {
        let user_info = &*(&*user_info as *const NSDictionary<NSString, NSNumber>
            as *const NSDictionary<AnyObject, AnyObject>);
        content.setUserInfo(user_info);
    }
    let identifier = NSProcessInfo::processInfo().globallyUniqueString();
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&identifier, &content, None);

    // Only the first call prompts the user; later calls answer with the stored decision right
    // away, so always posting from the handler avoids racing the first prompt.
    let post_center = center.clone();
    let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        if !granted.as_bool() {
            // SAFETY: The error is either null or a valid `NSError` for the handler's duration.
            let reason = unsafe { error.as_ref() }
                .map_or_else(|| "denied".into(), |error| error.localizedDescription().to_string());
            report_denied(&reason);
            return;
        }

        let done = RcBlock::new(|error: *mut NSError| {
            // SAFETY: The error is either null or a valid `NSError` for the handler's duration.
            if let Some(error) = unsafe { error.as_ref() } {
                warn!("Unable to post notification: {}", error.localizedDescription());
            }
        });
        post_center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    });

    center.requestAuthorizationWithOptions_completionHandler(AUTHORIZATION, &handler);
}

/// Ask once for the permission the Dock badge needs, which macOS also governs as a notification
/// setting ("Badge application icon"). Must only be called when [`is_available`] is `true`.
pub fn request_badge_permission() {
    static REQUESTED: AtomicBool = AtomicBool::new(false);
    if REQUESTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let handler = RcBlock::new(|granted: Bool, _error: *mut NSError| {
        debug!("Notification permission for the Dock badge granted: {}", granted.as_bool());
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(AUTHORIZATION, &handler);
}

/// Bring the window that posted the clicked notification to the front.
///
/// A window that closed since, or one from an earlier xaya process, simply isn't found.
fn focus_posting_window(response: &UNNotificationResponse) {
    let user_info = response.notification().request().content().userInfo();
    let key: &AnyObject = ns_string!("xayaWindow").as_ref();
    let Some(window) = user_info.objectForKey(key).and_then(|id| id.downcast::<NSNumber>().ok())
    else {
        return;
    };
    let window = WindowId::from(window.unsignedLongLongValue());
    if let Some(proxy) = PROXY.get() {
        let proxy = proxy.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = proxy.send_event(Event::new(EventType::FocusWindow, window));
    }
}

/// Tell the user once how to allow notifications; later denials only go to the debug log, so the
/// message bar isn't flooded by every notification the program sends.
fn report_denied(reason: &str) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        debug!("Notification dropped, not authorized: {reason}");
    } else {
        warn!(
            "Notifications are turned off for xaya ({reason}); allow them in System Settings → \
             Notifications → xaya"
        );
    }
}

/// Register the delegate that lets notifications show while xaya is the frontmost app.
///
/// Without it macOS silently drops them, which breaks `only_when_unfocused = false` and
/// notifications from a background window while another xaya window has focus.
fn install_delegate(mtm: MainThreadMarker, center: &UNUserNotificationCenter) {
    // The center only keeps a weak reference, so the delegate lives for the whole process.
    // `Retained` isn't `Sync`, so it's leaked rather than stored in a static.
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let delegate = NotificationDelegate::new(mtm);
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        std::mem::forget(delegate);
    });
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "XayaNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            focus_posting_window(response);
            completion_handler.call(());
        }

        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);

impl NotificationDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `NSObject`'s `init` takes no arguments and returns an initialized instance.
        unsafe { msg_send![super(this), init] }
    }
}
