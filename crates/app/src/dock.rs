//! The dock icon's menu: right-click zemacs in the dock, get **New Frame**.
//!
//! Safari's is the shape being copied — a left click on a running application's
//! dock icon activates it and nothing more, and the way you ask for a second
//! window is the menu under the right button. macOS builds that menu by asking
//! `NSApp`'s delegate for `applicationDockMenu:`, and SDL owns the delegate, so
//! the only way in is to add the method to SDL's delegate class at runtime.
//!
//! Done with the bare Objective-C runtime rather than a crate. `objc2` and
//! friends would be a dependency, a build-time cost and an API to track, for
//! four `extern "C"` declarations and one menu built once at startup. The
//! trade is that every `objc_msgSend` here has to be transmuted to a correctly
//! typed function pointer before it is called — arm64 passes arguments by
//! prototype, so calling the variadic symbol directly would put them in the
//! wrong registers. Hence one small helper per signature below rather than one
//! generic `send`.
//!
//! Nothing here touches the editor. The menu item sets a flag; the command loop
//! reads it with [`wanted`] and applies `NewFrame` itself. That is not
//! fastidiousness — AppKit runs this callback from inside SDL's event pumping,
//! which is *under* the editor lock, so an editor touched from here would
//! deadlock the first time anyone opened the menu.

use std::ffi::{c_char, c_void, CStr};
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

type Id = *mut c_void;
type Sel = *const c_void;
type Class = *mut c_void;

extern "C" {
    fn objc_getClass(name: *const c_char) -> Class;
    fn object_getClass(obj: Id) -> Class;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn class_addMethod(cls: Class, name: Sel, imp: *const c_void, types: *const c_char) -> bool;
    fn objc_msgSend();
}

/// The menu, built once at startup and handed out on every right click. Built
/// ahead of time because the callback runs on AppKit's terms: allocating there
/// would leak an autorelease-less menu per click, and there is nothing about
/// this menu that changes between them.
static MENU: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Set by the menu item, cleared by the command loop.
static WANTED: AtomicBool = AtomicBool::new(false);

/// Whether **New Frame** was chosen since the last call. Consumes the request.
pub fn wanted() -> bool {
    WANTED.swap(false, Ordering::Relaxed)
}

unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}

unsafe fn class(name: &CStr) -> Class {
    objc_getClass(name.as_ptr())
}

unsafe fn send(obj: Id, s: Sel) -> Id {
    let f: extern "C" fn(Id, Sel) -> Id = mem::transmute(objc_msgSend as *const c_void);
    f(obj, s)
}

unsafe fn send_id(obj: Id, s: Sel, a: Id) -> Id {
    let f: extern "C" fn(Id, Sel, Id) -> Id = mem::transmute(objc_msgSend as *const c_void);
    f(obj, s, a)
}

unsafe fn send_bool(obj: Id, s: Sel, a: bool) {
    let f: extern "C" fn(Id, Sel, bool) = mem::transmute(objc_msgSend as *const c_void);
    f(obj, s, a)
}

unsafe fn nsstring(text: &CStr) -> Id {
    let f: extern "C" fn(Id, Sel, *const c_char) -> Id =
        mem::transmute(objc_msgSend as *const c_void);
    f(
        class(c"NSString") as Id,
        sel(c"stringWithUTF8String:"),
        text.as_ptr(),
    )
}

/// `- (NSMenu *)applicationDockMenu:(NSApplication *)sender`
extern "C" fn dock_menu(_this: Id, _cmd: Sel, _sender: Id) -> Id {
    MENU.load(Ordering::Relaxed)
}

/// `- (void)zemacsNewFrame:(id)sender` — the menu item's action.
extern "C" fn new_frame(_this: Id, _cmd: Sel, _sender: Id) {
    WANTED.store(true, Ordering::Relaxed);
}

/// Call once, on the main thread, *after* SDL's video subsystem is up — that is
/// when SDL registers the application with Cocoa and installs the delegate this
/// hangs the menu off. Every failure here is silent and harmless: no delegate,
/// or a delegate that already answers `applicationDockMenu:`, means the dock
/// menu stays whatever it was and the rest of the editor does not care.
pub fn install() {
    unsafe {
        let app = send(class(c"NSApplication") as Id, sel(c"sharedApplication"));
        if app.is_null() {
            return;
        }
        let delegate = send(app, sel(c"delegate"));
        if delegate.is_null() {
            return;
        }
        let cls = object_getClass(delegate);

        // Both methods before the menu: the item's action is looked up on its
        // target, and `setAutoenablesItems:NO` below only decides whether AppKit
        // *asks* — an item whose target cannot answer is still dead on click.
        //
        // `v@:@` is "void, taking self and a selector and one object"; `@@:@`
        // the same returning an object. This is the type encoding the runtime
        // uses to lay the call out, and getting it wrong is how an argument
        // arrives as garbage.
        class_addMethod(
            cls,
            sel(c"zemacsNewFrame:"),
            new_frame as *const c_void,
            c"v@:@".as_ptr(),
        );
        if !class_addMethod(
            cls,
            sel(c"applicationDockMenu:"),
            dock_menu as *const c_void,
            c"@@:@".as_ptr(),
        ) {
            return; // SDL grew one of its own; leave it alone
        }

        let menu = send(send(class(c"NSMenu") as Id, sel(c"alloc")), sel(c"init"));
        let item = send(class(c"NSMenuItem") as Id, sel(c"alloc"));
        let init: extern "C" fn(Id, Sel, Id, Sel, Id) -> Id =
            mem::transmute(objc_msgSend as *const c_void);
        let item = init(
            item,
            sel(c"initWithTitle:action:keyEquivalent:"),
            nsstring(c"New Frame"),
            sel(c"zemacsNewFrame:"),
            nsstring(c""),
        );
        send_id(item, sel(c"setTarget:"), delegate);
        send_bool(item, sel(c"setEnabled:"), true);
        send_bool(menu, sel(c"setAutoenablesItems:"), false);
        send_id(menu, sel(c"addItem:"), item);
        // Kept for the life of the process on purpose — it is handed back to
        // AppKit on every click and released by nobody.
        MENU.store(menu, Ordering::Relaxed);
    }
}
