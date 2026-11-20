//! What the app draws over native web views, so they step aside.

use std::cell::RefCell;

use gpui::{AnyElement, Bounds, IntoElement, Pixels, Styled, canvas};

thread_local! {
    static COVERS: RefCell<Vec<Bounds<Pixels>>> = const { RefCell::new(Vec::new()) };
}

/// Forgets the covers of the last frame; call it before rendering the covers again.
pub fn clear_covers() {
    COVERS.with_borrow_mut(Vec::clear);
}

/// Placed inside a popup, dialog or window: web views under it hide while it is shown.
pub fn cover() -> AnyElement {
    canvas(
        |bounds, _, _| COVERS.with_borrow_mut(|covers| covers.push(bounds)),
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
    .into_any_element()
}

pub(crate) fn covered(bounds: Bounds<Pixels>) -> bool {
    COVERS.with_borrow(|covers| covers.iter().any(|cover| cover.intersects(&bounds)))
}
