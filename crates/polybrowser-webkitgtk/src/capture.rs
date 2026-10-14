use super::*;
pub(super) struct Capture {
    g: &'static Api,
    view: gpointer,
    host: gpointer,
    parent: gpointer,
    properties: Vec<(CString, ffi::Value)>,
    parent_closed: Rc<Cell<bool>>,
    parent_signal: std::ffi::c_ulong,
    requests: Weak<Requests>,
}
impl Capture {
    pub(super) unsafe fn new(
        g: &'static Api,
        view: gpointer,
        requests: Weak<Requests>,
    ) -> Result<Self> {
        unsafe {
            let mut width = 0;
            let mut height = 0;
            g.gtk_widget_get_size_request(view, &mut width, &mut height);
            width = width.max(g.gtk_widget_get_allocated_width(view));
            height = height.max(g.gtk_widget_get_allocated_height(view));
            if width <= 0 || height <= 0 || i64::from(width) * i64::from(height) > 64 * 1024 * 1024
            {
                return Err(error("invalid snapshot size"));
            }
            let mut capture = Self {
                g,
                view: g.g_object_ref(view),
                host: ptr::null_mut(),
                parent: ptr::null_mut(),
                properties: Vec::new(),
                parent_closed: Rc::new(Cell::new(false)),
                parent_signal: 0,
                requests,
            };
            if g.gtk_widget_get_mapped(view) == 0 {
                let parent = g.gtk_widget_get_parent(view);
                if !parent.is_null() {
                    capture.parent = g.g_object_ref(parent);
                    let mut count = 0;
                    let specs = g.gtk_container_class_list_child_properties(
                        *parent.cast::<gpointer>(),
                        &mut count,
                    );
                    for index in 0..count as usize {
                        let spec = &**specs.add(index);
                        if spec.flags & 3 != 3 {
                            continue;
                        }
                        let name = CStr::from_ptr(spec.name).to_owned();
                        let mut value = ffi::Value::default();
                        g.g_value_init(&mut value, spec.value_type);
                        g.gtk_container_child_get_property(parent, view, name.as_ptr(), &mut value);
                        capture.properties.push((name, value));
                    }
                    g.g_free(specs.cast());
                    let handler: ffi::Callback = std::mem::transmute(
                        on_parent_closed as unsafe extern "C" fn(gpointer, gpointer),
                    );
                    capture.parent_signal = g.g_signal_connect_data(
                        parent,
                        c"destroy".as_ptr(),
                        handler,
                        Box::into_raw(Box::new(capture.parent_closed.clone())).cast(),
                        free_parent_signal as *const () as gpointer,
                        0,
                    );
                    g.gtk_container_remove(parent, view);
                }
                let host = g.gtk_offscreen_window_new();
                g.gtk_widget_set_size_request(host, width, height);
                g.gtk_container_add(host, view);
                g.gtk_widget_show_all(host);
                g.gtk_widget_size_allocate(
                    view,
                    &ffi::Allocation {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                );
                capture.host = host;
            }
            Ok(capture)
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let g = self.g;
            if !self.host.is_null() {
                g.gtk_container_remove(self.host, self.view);
                if !self.parent.is_null() {
                    if self.requests.upgrade().is_some() && !self.parent_closed.get() {
                        g.gtk_container_add(self.parent, self.view);
                        for (name, value) in &self.properties {
                            g.gtk_container_child_set_property(
                                self.parent,
                                self.view,
                                name.as_ptr(),
                                value,
                            );
                        }
                    }
                    if g.g_signal_handler_is_connected(self.parent, self.parent_signal) != 0 {
                        g.g_signal_handler_disconnect(self.parent, self.parent_signal);
                    }
                    g.g_object_unref(self.parent);
                }
                g.gtk_widget_destroy(self.host);
            }
            if let Some(requests) = self.requests.upgrade() {
                if requests.visible.get() {
                    g.gtk_widget_show(self.view);
                } else {
                    g.gtk_widget_hide(self.view);
                }
            }
            for (_, value) in &mut self.properties {
                g.g_value_unset(value);
            }
            g.g_object_unref(self.view);
        }
    }
}
pub(super) struct Request {
    pub view: gpointer,
    pub cancel: gpointer,
    pub pending: Box<Pending<Image>>,
}
pub(super) unsafe extern "C" fn start(data: gpointer) -> c_int {
    let request = unsafe { Box::from_raw(data.cast::<Request>()) };
    if let Some(g) = ffi::loaded() {
        unsafe {
            g.webkit_web_view_get_snapshot(
                request.view,
                0,
                0,
                request.cancel,
                callbacks::on_snapshot,
                Box::into_raw(request.pending).cast(),
            );
            g.g_object_unref(request.cancel);
            g.g_object_unref(request.view);
        }
    }
    0
}

unsafe extern "C" fn on_parent_closed(_parent: gpointer, data: gpointer) {
    unsafe { &*data.cast::<Rc<Cell<bool>>>() }.set(true);
}
unsafe extern "C" fn free_parent_signal(data: gpointer, _closure: gpointer) {
    drop(unsafe { Box::from_raw(data.cast::<Rc<Cell<bool>>>()) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "Requires a GTK display; run with xvfb-run"]
    fn hidden_capture_preserves_container_properties_and_current_visibility() {
        unsafe {
            let gtk = libloading::Library::new("libgtk-3.so.0").unwrap();
            let init = gtk
                .get::<unsafe extern "C" fn(gpointer, gpointer) -> c_int>(b"gtk_init_check\0")
                .unwrap();
            assert_ne!(init(ptr::null_mut(), ptr::null_mut()), 0);
            let fixed_new = gtk
                .get::<unsafe extern "C" fn() -> gpointer>(b"gtk_fixed_new\0")
                .unwrap();
            let button_new = gtk
                .get::<unsafe extern "C" fn() -> gpointer>(b"gtk_button_new\0")
                .unwrap();
            let put = gtk
                .get::<unsafe extern "C" fn(gpointer, gpointer, c_int, c_int)>(b"gtk_fixed_put\0")
                .unwrap();
            let g = ffi::load().unwrap();
            let host = g.gtk_offscreen_window_new();
            let fixed = fixed_new();
            let view = button_new();
            g.gtk_container_add(host, fixed);
            put(fixed, view, 31, 47);
            g.gtk_widget_set_size_request(view, 320, 240);
            g.gtk_widget_show(view);
            let requests = Rc::new(Requests {
                visible: Cell::new(true),
                ..Requests::default()
            });
            let capture = Capture::new(g, view, Rc::downgrade(&requests)).unwrap();
            requests.visible.set(false);
            drop(capture);
            assert_eq!(g.gtk_widget_get_parent(view), fixed);
            assert_eq!(g.gtk_widget_get_visible(view), 0);
            for (name, expected) in [(c"x", 31), (c"y", 47)] {
                let mut value = ffi::Value::default();
                g.g_value_init(&mut value, 6 << 2);
                g.gtk_container_child_get_property(fixed, view, name.as_ptr(), &mut value);
                assert_eq!(value.data[0] as i32, expected);
                g.g_value_unset(&mut value);
            }
            g.gtk_widget_destroy(host);
        }
    }
}
