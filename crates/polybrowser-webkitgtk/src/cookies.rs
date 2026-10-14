use super::*;
use std::{ffi::CString, time::Duration};
struct Listing {
    context: Rc<SiteData>,
    filtered: bool,
    done: Done<Vec<Cookie>>,
}
pub(super) fn list(
    g: &'static Api,
    context: Rc<SiteData>,
    url: Option<&str>,
    done: Done<Vec<Cookie>>,
) {
    let manager = unsafe { g.webkit_web_context_get_cookie_manager(context.native) };
    let pending = Box::new(Listing {
        context,
        filtered: url.is_some(),
        done,
    });
    unsafe {
        if let Some(url) = url {
            g.webkit_cookie_manager_get_cookies(
                manager,
                cstring(url).as_ptr(),
                ptr::null_mut(),
                on_list,
                Box::into_raw(pending).cast(),
            );
        } else {
            g.webkit_cookie_manager_get_all_cookies(
                manager,
                ptr::null_mut(),
                on_list,
                Box::into_raw(pending).cast(),
            );
        }
    }
}
unsafe extern "C" fn on_list(manager: gpointer, result: gpointer, data: gpointer) {
    let pending = unsafe { Box::from_raw(data.cast::<Listing>()) };
    let Some(g) = ffi::loaded() else {
        return;
    };
    let mut failure = ptr::null_mut();
    let head = unsafe {
        if pending.filtered {
            g.webkit_cookie_manager_get_cookies_finish(manager, result, &raw mut failure)
        } else {
            g.webkit_cookie_manager_get_all_cookies_finish(manager, result, &raw mut failure)
        }
    };
    let mut cookies = Vec::new();
    let mut item = head;
    unsafe {
        while !item.is_null() {
            cookies.push(read(g, (*item).data));
            g.soup_cookie_free((*item).data);
            item = (*item).next;
        }
        g.g_list_free(head);
    }
    let _keep = pending.context;
    (pending.done)(callbacks::take_error(g, failure).map_or(Ok(cookies), Err));
}
unsafe fn read(g: &Api, native: gpointer) -> Cookie {
    unsafe {
        let expires = g.soup_cookie_get_expires(native);
        Cookie {
            name: text(g.soup_cookie_get_name(native)).unwrap_or_default(),
            value: text(g.soup_cookie_get_value(native)).unwrap_or_default(),
            domain: text(g.soup_cookie_get_domain(native)).unwrap_or_default(),
            path: text(g.soup_cookie_get_path(native)).unwrap_or_default(),
            secure: g.soup_cookie_get_secure(native) != 0,
            http_only: g.soup_cookie_get_http_only(native) != 0,
            expires: (!expires.is_null())
                .then(|| g.g_date_time_to_unix(expires))
                .and_then(|seconds| u64::try_from(seconds).ok())
                .map(|seconds| (std::time::UNIX_EPOCH + Duration::from_secs(seconds)).into()),
        }
    }
}
fn native_cookie(g: &Api, cookie: &Cookie) -> Result<gpointer> {
    let parts = [&cookie.name, &cookie.value, &cookie.domain, &cookie.path]
        .map(|value| CString::new(value.as_str()));
    let [name, value, domain, path] = parts;
    let (name, value, domain, path) = (
        name.map_err(|_| error("invalid cookie name"))?,
        value.map_err(|_| error("invalid cookie value"))?,
        domain.map_err(|_| error("invalid cookie domain"))?,
        path.map_err(|_| error("invalid cookie path"))?,
    );
    unsafe {
        let native = g.soup_cookie_new(
            name.as_ptr(),
            value.as_ptr(),
            domain.as_ptr(),
            path.as_ptr(),
            -1,
        );
        if native.is_null() {
            return Err(error("invalid cookie"));
        }
        g.soup_cookie_set_secure(native, i32::from(cookie.secure));
        g.soup_cookie_set_http_only(native, i32::from(cookie.http_only));
        if let Some(expires) = cookie
            .expires
            .and_then(|value| {
                std::time::SystemTime::from(value)
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
            })
            .and_then(|value| i64::try_from(value.as_secs()).ok())
        {
            let date = g.g_date_time_new_from_unix_utc(expires);
            if !date.is_null() {
                g.soup_cookie_set_expires(native, date);
                g.g_date_time_unref(date);
            }
        }
        Ok(native)
    }
}
struct Changed {
    context: Rc<SiteData>,
    delete: bool,
    done: Done<()>,
}
pub(super) fn change(
    g: &'static Api,
    context: Rc<SiteData>,
    cookie: &Cookie,
    delete: bool,
    done: Done<()>,
) {
    let native = match native_cookie(g, cookie) {
        Ok(native) => native,
        Err(failure) => {
            done(Err(failure));
            return;
        }
    };
    let manager = unsafe { g.webkit_web_context_get_cookie_manager(context.native) };
    let pending = Box::new(Changed {
        context,
        delete,
        done,
    });
    unsafe {
        if delete {
            g.webkit_cookie_manager_delete_cookie(
                manager,
                native,
                ptr::null_mut(),
                on_changed,
                Box::into_raw(pending).cast(),
            );
        } else {
            g.webkit_cookie_manager_add_cookie(
                manager,
                native,
                ptr::null_mut(),
                on_changed,
                Box::into_raw(pending).cast(),
            );
        }
        g.soup_cookie_free(native);
    }
}
unsafe extern "C" fn on_changed(manager: gpointer, result: gpointer, data: gpointer) {
    let pending = unsafe { Box::from_raw(data.cast::<Changed>()) };
    let Some(g) = ffi::loaded() else {
        return;
    };
    let mut failure = ptr::null_mut();
    let ok = unsafe {
        if pending.delete {
            g.webkit_cookie_manager_delete_cookie_finish(manager, result, &raw mut failure)
        } else {
            g.webkit_cookie_manager_add_cookie_finish(manager, result, &raw mut failure)
        }
    };
    let _keep = pending.context;
    (pending.done)(callbacks::take_error(g, failure).map_or_else(
        || {
            if ok == 0 {
                Err(error("cookie update failed"))
            } else {
                Ok(())
            }
        },
        Err,
    ));
}
pub(super) fn sequence(
    g: &'static Api,
    context: Rc<SiteData>,
    mut cookies: std::vec::IntoIter<Cookie>,
    delete: bool,
    count: usize,
    done: Done<usize>,
) {
    let Some(cookie) = cookies.next() else {
        done(Ok(count));
        return;
    };
    change(
        g,
        context.clone(),
        &cookie,
        delete,
        Box::new(move |result| match result {
            Ok(()) => sequence(g, context, cookies, delete, count + 1, done),
            Err(failure) => done(Err(failure)),
        }),
    );
}
struct Cleared {
    context: Rc<SiteData>,
    done: Done<()>,
}
pub(super) fn clear(g: &'static Api, context: Rc<SiteData>, types: u32, done: Done<()>) {
    let manager = unsafe { g.webkit_web_context_get_website_data_manager(context.native) };
    let pending = Box::new(Cleared { context, done });
    unsafe {
        g.webkit_website_data_manager_clear(
            manager,
            types,
            0,
            ptr::null_mut(),
            on_clear,
            Box::into_raw(pending).cast(),
        );
    }
}
unsafe extern "C" fn on_clear(manager: gpointer, result: gpointer, data: gpointer) {
    let pending = unsafe { Box::from_raw(data.cast::<Cleared>()) };
    let Some(g) = ffi::loaded() else {
        return;
    };
    let mut failure = ptr::null_mut();
    let ok =
        unsafe { g.webkit_website_data_manager_clear_finish(manager, result, &raw mut failure) };
    let _keep = pending.context;
    (pending.done)(callbacks::take_error(g, failure).map_or_else(
        || {
            if ok == 0 {
                Err(error("clearing browser data failed"))
            } else {
                Ok(())
            }
        },
        Err,
    ));
}
