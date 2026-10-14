#![allow(non_camel_case_types)]
use libloading::Library;
use std::{
    ffi::{c_char, c_int, c_uint, c_ulong, c_void},
    sync::OnceLock,
};
pub(super) type gpointer = *mut c_void;
pub(super) type gboolean = c_int;
pub(super) type SourceFunc = unsafe extern "C" fn(gpointer) -> gboolean;
pub(super) type DestroyNotify = unsafe extern "C" fn(gpointer);
pub(super) type Callback = unsafe extern "C" fn();
pub(super) type AsyncReadyCallback = unsafe extern "C" fn(gpointer, gpointer, gpointer);
#[repr(C)]
pub(super) struct GList {
    pub data: gpointer,
    pub next: *mut GList,
    pub previous: *mut GList,
}
#[repr(C)]
pub(super) struct GError {
    pub domain: u32,
    pub code: c_int,
    pub message: *mut c_char,
}
#[repr(C)]
pub(super) struct Allocation {
    pub x: c_int,
    pub y: c_int,
    pub width: c_int,
    pub height: c_int,
}
#[repr(C)]
pub(super) struct ParamSpec {
    pub class: gpointer,
    pub name: *const c_char,
    pub flags: c_uint,
    pub value_type: usize,
}
#[repr(C)]
#[derive(Default)]
pub(super) struct Value {
    pub kind: usize,
    pub data: [u64; 2],
}
macro_rules! libraries {
    ($($library:literal { $(fn $name:ident($($argument:ident: $kind:ty),*) $(-> $result:ty)?;)* })*) => {
        pub(super) struct Api {
            $($($name: unsafe extern "C" fn($($kind),*) $(-> $result)?,)*)*
            _libraries: Vec<Library>,
        }

        impl Api {
            fn bind() -> Result<Self, libloading::Error> {
                let mut libraries = Vec::new();
                // SAFETY: Loading runs the libraries' initializers, which only
                // set up their own state. Each symbol is the C function of that
                // name, with the signature its headers give it.
                unsafe {
                    $(
                        let library = Library::new($library)?;
                        $(let $name = *library.get(concat!(stringify!($name), "\0").as_bytes())?;)*
                        libraries.push(library);
                    )*
                    Ok(Self {
                        $($($name,)*)*
                        _libraries: libraries,
                    })
                }
            }

            $($(
                /// # Safety
                ///
                /// The arguments are what the C function of this name
                /// requires, and it is called on the thread that runs GTK.
                #[allow(clippy::too_many_arguments)]
                pub(super) unsafe fn $name(&self, $($argument: $kind),*) $(-> $result)? {
                    // SAFETY: Upheld by the caller.
                    unsafe { (self.$name)($($argument),*) }
                }
            )*)*
        }
    };
}

libraries! {
    "libglib-2.0.so.0" {
        fn g_free(memory: gpointer);
        fn g_timeout_add_full(priority: c_int, interval: c_uint, function: SourceFunc, data: gpointer, notify: Option<DestroyNotify>) -> c_uint;
        fn g_source_remove(id: c_uint) -> gboolean;
        fn g_list_free(list: *mut GList);
        fn g_date_time_to_unix(date: gpointer) -> i64;
        fn g_date_time_new_from_unix_utc(seconds: i64) -> gpointer;
        fn g_date_time_unref(date: gpointer);
        fn g_error_free(error: *mut GError);
    }
    "libgio-2.0.so.0" {
        fn g_cancellable_new() -> gpointer;
        fn g_cancellable_cancel(cancellable: gpointer);
        fn g_cancellable_is_cancelled(cancel: gpointer) -> gboolean;
    }
    "libgobject-2.0.so.0" {
        fn g_value_init(value: *mut Value, kind: usize) -> *mut Value;
        fn g_value_unset(value: *mut Value);
        fn g_object_ref(object: gpointer) -> gpointer;
        fn g_object_unref(object: gpointer);
        fn g_signal_connect_data(
            instance: gpointer,
            signal: *const c_char,
            handler: Callback,
            data: gpointer,
            destroy: gpointer,
            flags: c_uint
        ) -> c_ulong;
        fn g_signal_handler_disconnect(object: gpointer, id: c_ulong);
        fn g_signal_handler_is_connected(object: gpointer, id: c_ulong) -> gboolean;
    }
    "libcairo.so.2" {
        fn cairo_surface_flush(surface: gpointer);
        fn cairo_surface_destroy(surface: gpointer);
        fn cairo_surface_map_to_image(surface: gpointer, extents: gpointer) -> gpointer;
        fn cairo_surface_unmap_image(surface: gpointer, image: gpointer);
        fn cairo_surface_status(surface: gpointer) -> c_int;
        fn cairo_image_surface_get_data(image: gpointer) -> *mut u8;
        fn cairo_image_surface_get_format(image: gpointer) -> c_int;
        fn cairo_image_surface_get_width(image: gpointer) -> c_int;
        fn cairo_image_surface_get_height(image: gpointer) -> c_int;
        fn cairo_image_surface_get_stride(image: gpointer) -> c_int;
    }
    "libgtk-3.so.0" {
        fn gtk_widget_show(widget: gpointer);
        fn gtk_container_class_list_child_properties(class: gpointer, count: *mut c_uint) -> *mut *mut ParamSpec;
        fn gtk_container_child_get_property(container: gpointer, child: gpointer, name: *const c_char, value: *mut Value);
        fn gtk_container_child_set_property(container: gpointer, child: gpointer, name: *const c_char, value: *const Value);
        fn gtk_offscreen_window_new() -> gpointer;
        fn gtk_container_add(container: gpointer, widget: gpointer);
        fn gtk_widget_hide(widget: gpointer);
        fn gtk_widget_destroy(widget: gpointer);
        fn gtk_widget_set_size_request(widget: gpointer, width: c_int, height: c_int);
        fn gtk_widget_get_visible(widget: gpointer) -> gboolean;
        fn gtk_widget_get_mapped(widget: gpointer) -> gboolean;
        fn gtk_widget_get_parent(widget: gpointer) -> gpointer;
        fn gtk_widget_get_allocated_width(widget: gpointer) -> c_int;
        fn gtk_widget_get_allocated_height(widget: gpointer) -> c_int;
        fn gtk_widget_get_size_request(widget: gpointer, width: *mut c_int, height: *mut c_int);
        fn gtk_container_remove(container: gpointer, widget: gpointer);
        fn gtk_widget_show_all(widget: gpointer);
        fn gtk_widget_size_allocate(widget: gpointer, allocation: *const Allocation);
    }
    "libsoup-3.0.so.0" {
        fn soup_cookie_new(name: *const c_char, value: *const c_char, domain: *const c_char, path: *const c_char, max_age: c_int) -> gpointer;
        fn soup_cookie_free(cookie: gpointer);
        fn soup_cookie_get_name(cookie: gpointer) -> *const c_char;
        fn soup_cookie_get_value(cookie: gpointer) -> *const c_char;
        fn soup_cookie_get_domain(cookie: gpointer) -> *const c_char;
        fn soup_cookie_get_path(cookie: gpointer) -> *const c_char;
        fn soup_cookie_get_secure(cookie: gpointer) -> gboolean;
        fn soup_cookie_get_http_only(cookie: gpointer) -> gboolean;
        fn soup_cookie_get_expires(cookie: gpointer) -> gpointer;
        fn soup_cookie_set_secure(cookie: gpointer, secure: gboolean);
        fn soup_cookie_set_http_only(cookie: gpointer, http_only: gboolean);
        fn soup_cookie_set_expires(cookie: gpointer, expires: gpointer);
    }
    "libjavascriptcoregtk-4.1.so.0" {
        fn jsc_value_is_string(value: gpointer) -> gboolean;
        fn jsc_value_to_string(value: gpointer) -> *mut c_char;
    }
    "libwebkit2gtk-4.1.so.0" {
        fn webkit_web_context_get_website_data_manager(context: gpointer) -> gpointer;
        fn webkit_web_context_get_cookie_manager(context: gpointer) -> gpointer;
        fn webkit_cookie_manager_get_all_cookies(manager: gpointer, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_cookie_manager_get_all_cookies_finish(manager: gpointer, result: gpointer, error: *mut *mut GError) -> *mut GList;
        fn webkit_cookie_manager_get_cookies(manager: gpointer, uri: *const c_char, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_cookie_manager_get_cookies_finish(manager: gpointer, result: gpointer, error: *mut *mut GError) -> *mut GList;
        fn webkit_cookie_manager_add_cookie(manager: gpointer, cookie: gpointer, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_cookie_manager_add_cookie_finish(manager: gpointer, result: gpointer, error: *mut *mut GError) -> gboolean;
        fn webkit_cookie_manager_delete_cookie(manager: gpointer, cookie: gpointer, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_cookie_manager_delete_cookie_finish(manager: gpointer, result: gpointer, error: *mut *mut GError) -> gboolean;
        fn webkit_website_data_manager_clear(manager: gpointer, types: u32, timespan: i64, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_website_data_manager_clear_finish(manager: gpointer, result: gpointer, error: *mut *mut GError) -> gboolean;
        fn webkit_web_view_call_async_javascript_function(view: gpointer, body: *const c_char, length: isize, args: gpointer, world: *const c_char, source: *const c_char, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_web_view_call_async_javascript_function_finish(view: gpointer, result: gpointer, error: *mut *mut GError) -> gpointer;
        fn webkit_web_view_get_snapshot(view: gpointer, region: c_int, options: c_uint, cancel: gpointer, callback: AsyncReadyCallback, data: gpointer);
        fn webkit_web_view_get_snapshot_finish(view: gpointer, result: gpointer, error: *mut *mut GError) -> gpointer;
        fn webkit_web_view_get_context(view: gpointer) -> gpointer;
    }
}

static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
pub(super) fn load() -> crate::Result<&'static Api> {
    API.get_or_init(|| Api::bind().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(crate::error)
}
pub(super) fn loaded() -> Option<&'static Api> {
    API.get().and_then(|api| api.as_ref().ok())
}
