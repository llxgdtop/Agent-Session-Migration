//! Native macOS menu bar (compiled for macOS only).
//!
//! eframe/egui cannot draw into the system-wide menu bar at the top of the
//! screen, so on macOS the app menu is wired through AppKit instead: the
//! in-window egui menu bar is not drawn (see `main.rs`) and the entries
//! live next to the Apple menu, the way a macOS user expects.
//!
//! What ends up in the menu bar:
//!
//! - winit installs a default menu (About, Services, Hide, Quit) before
//!   eframe's app-creation callback runs; this module augments it in
//!   place: the top-level item is retitled to the app name, "Settings…"
//!   (⌘,) is inserted at the top of the app menu, and winit's Quit item
//!   is retitled and re-targeted so quitting goes through egui (below).
//!   Keeping winit's entries preserves standard behavior (About panel,
//!   Services, Hide) with zero duplication of AppKit boilerplate.
//! - If that default menu ever disappears (a winit change), a minimal
//!   replacement (Settings…, separator, Quit) is installed instead.
//! - Language switching deliberately stays in the shared Settings popup:
//!   native menu items would need their titles and checkmarks refreshed
//!   on every language change, which is not worth the bridge surface for
//!   a three-way choice that already exists in the popup (shared with
//!   Windows/Linux). The menu titles are therefore fixed English.
//!
//! # Bridge design (AppKit -> egui)
//!
//! AppKit delivers menu picks as Objective-C action messages, which cannot
//! reach into the egui app state. Each action therefore only flips a
//! static [`AtomicBool`] and requests a repaint through a stored
//! [`egui::Context`] clone — a menu pick is an AppKit event that egui
//! would not otherwise see, so without the repaint nudge the flag would
//! sit unnoticed until the next timed repaint. The egui frame loop polls
//! the flags once per frame, applies the effect through the normal egui
//! code path and clears them:
//!
//! - [`SETTINGS_REQUESTED`] -> `HubApp::show_settings = true`
//! - [`EXIT_REQUESTED`] -> [`egui::ViewportCommand::Close`]. Quitting via
//!   `NSApplication::terminate` would skip eframe's teardown (storage
//!   save, drop of app state), so "Quit" routes through the viewport
//!   close command instead; this also keeps ⌘Q identical to closing the
//!   window.
//!
//! # Unsafe boundaries
//!
//! All Objective-C interop of the app lives in this module; each `unsafe`
//! block below carries its own justification. The overall invariants:
//!
//! - `define_class!` registers `MenuTarget` as an NSObject subclass. The
//!   class has no ivars, does not implement `Drop`, and both action
//!   methods only write atomics and call the thread-safe
//!   [`egui::Context::request_repaint`], so no AppKit call schedule can
//!   break memory safety.
//! - The single `MenuTarget` instance is deliberately leaked: menu items
//!   do not retain their target, so the object must outlive them (i.e.
//!   the whole process).
//! - Everything runs on the main thread, guarded by [`MainThreadMarker`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, ClassType, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::{NSObject, NSString};

/// Title of the app menu (the bold entry right of the Apple menu); also
/// used by the quit item ("Quit {APP_NAME}"). Language-neutral product
/// name, not translated (see the i18n module docs).
const APP_NAME: &str = "Agent Session Migration";

/// Set by the native "Settings…" item; polled and cleared once per frame.
static SETTINGS_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set by the native "Quit" item; polled and cleared once per frame.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Repaint handle for the action methods (see module docs).
/// [`egui::Context`] is `Send + Sync` and cheap to clone, which is what
/// makes the flag-plus-repaint bridge sound from an AppKit call site.
static REPAINT_CTX: OnceLock<egui::Context> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements, `MenuTarget` has
    // no ivars and does not implement `Drop`, and both action methods only
    // touch atomics and the thread-safe repaint handle.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "HubMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        /// Action of the "Settings…" item (selector `hubOpenSettings:`).
        #[unsafe(method(hubOpenSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            SETTINGS_REQUESTED.store(true, Ordering::Release);
            request_repaint();
        }

        /// Action of the "Quit" item (selector `hubQuit:`).
        #[unsafe(method(hubQuit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            EXIT_REQUESTED.store(true, Ordering::Release);
            request_repaint();
        }
    }
);

impl MenuTarget {
    /// Allocate and initialize the single instance (no ivars to set up).
    ///
    /// The class is main-thread-only, so the caller must hold a
    /// [`MainThreadMarker`]; `install` does and is this function's only
    /// caller.
    fn new() -> Retained<Self> {
        // SAFETY: `new` (`alloc` + `init`) on NSObject has the expected
        // signature and fully constructs the object — the subclass adds
        // no ivars and overrides no initializers — and the message is
        // sent from the main thread (see the function docs).
        unsafe { msg_send![MenuTarget::class(), new] }
    }
}

/// Wake the egui UI so a flag set right before this is observed on the
/// very next frame instead of at the next timed repaint.
fn request_repaint() {
    if let Some(ctx) = REPAINT_CTX.get() {
        ctx.request_repaint();
    }
}

/// Consume a pending "open settings" request from the native menu.
pub fn take_settings_requested() -> bool {
    SETTINGS_REQUESTED.swap(false, Ordering::AcqRel)
}

/// Consume a pending "quit" request from the native menu.
pub fn take_exit_requested() -> bool {
    EXIT_REQUESTED.swap(false, Ordering::AcqRel)
}

/// Install/augment the native menu bar. Call once from eframe's app
/// creation callback: by then winit has initialized `NSApplication` and
/// its default menu, and everything here must run on the main thread.
pub fn install(egui_ctx: &egui::Context) {
    let Some(mtm) = MainThreadMarker::new() else {
        // eframe always invokes the creation callback on the main thread;
        // if that ever stopped holding, AppKit would be unusable from
        // here anyway - degrade to "no native menu" instead of panicking.
        return;
    };
    let _ = REPAINT_CTX.set(egui_ctx.clone());

    let app = NSApplication::sharedApplication(mtm);

    // SAFETY: casting to `AnyObject` is always sound - every Objective-C
    // object is one. The erase is only needed because `setTarget` takes
    // `Option<&AnyObject>`, not a reference to the concrete class.
    let target = unsafe { Retained::cast_unchecked::<AnyObject>(MenuTarget::new()) };

    match app.mainMenu() {
        // winit's default menu exists (the normal path): customize it.
        Some(menu) => customize_default_menu(&menu, &target, mtm),
        // Unexpected (winit changed its menu setup): install our own.
        None => install_minimal_menu(&app, &target, mtm),
    }

    // SAFETY justification for the leak (not an unsafe block, but the
    // reason `target` can never drop): NSMenuItem does not retain its
    // target, so releasing the only owned reference would leave the menu
    // items with a dangling pointer. Deliberately leak the instance so it
    // outlives the menu, i.e. the whole process.
    std::mem::forget(target);
}

/// Retitle the top-level item to the app name, insert "Settings…" at the
/// top of the app menu and reroute winit's Quit item through the exit
/// flag. All other default entries (About, Services, Hide, …) are kept.
fn customize_default_menu(menu: &NSMenu, target: &AnyObject, mtm: MainThreadMarker) {
    let Some(app_item) = menu.itemAtIndex(0) else {
        return;
    };
    app_item.setTitle(&NSString::from_str(APP_NAME));
    let Some(app_menu) = app_item.submenu() else {
        return;
    };
    app_menu.insertItem_atIndex(&settings_item(target, mtm), 0);
    retarget_default_items(&app_menu, target, mtm);
}

/// Adjust winit's default app-menu entries to this app: About/Hide/Quit
/// are retitled with the product name (winit uses the process name,
/// "hub-app"), and Quit is rerouted through the exit flag so ⌘Q runs
/// through eframe's regular teardown instead of
/// `NSApplication.terminate`. If no quit item exists (unexpected), one is
/// appended.
fn retarget_default_items(app_menu: &NSMenu, target: &AnyObject, mtm: MainThreadMarker) {
    let mut quit_retargeted = false;
    for index in 0..app_menu.numberOfItems() {
        let Some(item) = app_menu.itemAtIndex(index) else {
            continue;
        };
        match item.action() {
            // "About <process name>" -> "About Agent Session Migration"
            Some(action) if action == sel!(orderFrontStandardAboutPanel:) => {
                item.setTitle(&NSString::from_str(&format!("About {APP_NAME}")));
            }
            // "Hide <process name>" -> "Hide Agent Session Migration"
            Some(action) if action == sel!(hide:) => {
                item.setTitle(&NSString::from_str(&format!("Hide {APP_NAME}")));
            }
            // "Quit <process name>" (terminate:) -> flag-based quit
            Some(action) if action == sel!(terminate:) => {
                item.setTitle(&NSString::from_str(&format!("Quit {APP_NAME}")));
                // SAFETY: `hubQuit:` is registered on MenuTarget by
                // `define_class!` (the class was instantiated by
                // `install` before this runs) and has the action
                // signature (`v@:@`), and `target` points to an
                // instance that lives for the whole process (see
                // `install`).
                unsafe {
                    item.setAction(Some(sel!(hubQuit:)));
                    item.setTarget(Some(target));
                }
                quit_retargeted = true;
            }
            _ => {}
        }
    }
    if !quit_retargeted {
        app_menu.addItem(&quit_item(target, mtm));
    }
}

/// Fallback used only if winit no longer installs its default menu: a
/// menu bar with a single app menu holding Settings, a separator and
/// Quit.
fn install_minimal_menu(app: &NSApplication, target: &AnyObject, mtm: MainThreadMarker) {
    let app_menu = NSMenu::new(mtm);
    app_menu.addItem(&settings_item(target, mtm));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&quit_item(target, mtm));

    let app_item = NSMenuItem::new(mtm);
    app_item.setTitle(&NSString::from_str(APP_NAME));
    app_item.setSubmenu(Some(&app_menu));

    let main_menu = NSMenu::new(mtm);
    main_menu.addItem(&app_item);
    app.setMainMenu(Some(&main_menu));
}

/// "Settings…" item bound to `hubOpenSettings:` with the ⌘, equivalent.
fn settings_item(target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    action_item("Settings…", ",", sel!(hubOpenSettings:), target, mtm)
}

/// "Quit {APP_NAME}" item bound to `hubQuit:` with the ⌘Q equivalent.
fn quit_item(target: &AnyObject, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    action_item(
        &format!("Quit {APP_NAME}"),
        "q",
        sel!(hubQuit:),
        target,
        mtm,
    )
}

/// One action menu item with a ⌘ key equivalent (a fresh item's default
/// modifier mask is already Command).
fn action_item(
    title: &str,
    key: &str,
    action: Sel,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    // SAFETY: the designated initializer of NSMenuItem has the expected
    // signature; the action selector is registered on MenuTarget by
    // `define_class!` (whose class was instantiated by `install` before
    // any of these helpers run), and the target lives for the whole
    // process (see `install`).
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key),
        )
    };
    // SAFETY: same target/selector justification as above.
    unsafe { item.setTarget(Some(target)) };
    item
}
