//! The service picker itself: building one, filling it, and finding the one a window is showing.
//!
//! This is the whole of the toolbar's dealings with `AppKit` views, kept apart from the delegate
//! that drives them because none of it needs anything else the app knows: a picker is built from a
//! list of names and a selection, and found again through the window it was handed to. Nothing
//! here reads a selection back out, which is what lets the delegate settle one before any picker
//! exists and then give the same answer to both a new picker and the frontend.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSPopUpButton, NSWindow};
use objc2_foundation::{NSRect, NSString};

/// Builds a picker offering `names`, with `selected` picked.
///
/// The frame is zero because a toolbar item sizes its view from that view's own intrinsic content
/// size, so a frame set here would only be overwritten.
pub fn build(
    mtm: MainThreadMarker,
    names: &[String],
    selected: Option<&str>,
) -> Retained<NSPopUpButton> {
    let picker = NSPopUpButton::initWithFrame_pullsDown(mtm.alloc(), NSRect::ZERO, false);
    fill(&picker, names, selected);
    picker
}

/// Replaces `picker`'s entries with `names`, picking `selected`.
///
/// A `selected` that `names` does not offer leaves the picker with nothing picked at all, rather
/// than falling back to the first entry: `NSPopUpButton` deselects when it is asked for a title it
/// does not hold. Callers settle the selection against the same list beforehand, so that state is
/// not one the toolbar ever ends up in.
pub fn fill(picker: &NSPopUpButton, names: &[String], selected: Option<&str>) {
    picker.removeAllItems();
    for name in names {
        picker.addItemWithTitle(&NSString::from_str(name));
    }
    if let Some(selected) = selected {
        picker.selectItemWithTitle(&NSString::from_str(selected));
    }
}

/// The picker inside the toolbar item `window` holds under `identifier`.
///
/// Asked of the window rather than remembered: a toolbar owns the items it was handed, so it is
/// the only thing that knows which of them it kept.
pub fn find(window: &NSWindow, identifier: &str) -> Option<Retained<NSPopUpButton>> {
    window
        .toolbar()?
        .items()
        .iter()
        .find(|item| item.itemIdentifier().to_string() == identifier)
        .and_then(|item| item.view())
        .and_then(|view| view.downcast::<NSPopUpButton>().ok())
}
