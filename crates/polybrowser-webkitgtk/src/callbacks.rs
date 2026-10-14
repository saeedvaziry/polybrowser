use super::*;
pub(super) unsafe extern "C" fn on_script(view: gpointer, result: gpointer, data: gpointer) {
    let pending = unsafe { Box::from_raw(data.cast::<Pending<String>>()) };
    let Some(g) = ffi::loaded() else {
        pending.finish(Err(error("browser engine unavailable")));
        return;
    };
    let mut failure = ptr::null_mut();
    let value = unsafe {
        g.webkit_web_view_call_async_javascript_function_finish(view, result, &raw mut failure)
    };
    if let Some(failure) = take_error(g, failure) {
        pending.finish(Err(failure));
        return;
    }
    if value.is_null() {
        pending.finish(Err(error("script returned no result")));
        return;
    }
    let result = unsafe {
        let text = if g.jsc_value_is_string(value) != 0 {
            owned(g, g.jsc_value_to_string(value))
        } else {
            None
        };
        g.g_object_unref(value);
        text.ok_or_else(|| error("script returned no JSON"))
    };
    pending.finish(result);
}
pub(super) fn take_error(g: &Api, failure: *mut GError) -> Option<polybrowser::Error> {
    if failure.is_null() {
        return None;
    }
    unsafe {
        let result = error(text((*failure).message).unwrap_or_default());
        g.g_error_free(failure);
        Some(result)
    }
}
pub(super) unsafe extern "C" fn on_snapshot(view: gpointer, result: gpointer, data: gpointer) {
    let pending = unsafe { Box::from_raw(data.cast::<Pending<Image>>()) };
    let Some(g) = ffi::loaded() else {
        pending.finish(Err(error("browser engine unavailable")));
        return;
    };
    let mut failure = ptr::null_mut();
    let surface = unsafe { g.webkit_web_view_get_snapshot_finish(view, result, &raw mut failure) };
    if let Some(failure) = take_error(g, failure) {
        pending.finish(Err(failure));
        return;
    }
    if surface.is_null() {
        pending.finish(Err(error("empty screenshot")));
        return;
    }
    let image = unsafe { pixels::snapshot_pixels(g, surface) };
    unsafe {
        g.cairo_surface_destroy(surface);
    }
    pending.finish(image);
}
