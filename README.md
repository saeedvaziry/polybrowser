# polybrowser

A web browser for any Rust desktop application.

`polybrowser` does not implement a browser engine. It embeds the ones that
already exist behind one API, and adds the parts of a browser that sit above
the engine: profiles, history, bookmarks, and import from the browsers
installed on the machine.

```rust
use polybrowser::{Browser, Engine, ProfileManager, Rect};

fn main() -> polybrowser::Result<()> {
    polybrowser::init(); // first thing in main

    let profiles = ProfileManager::open("com.example.myapp")?;
    let profile = profiles.get_or_create("Default")?;

    // `window` is any window that implements raw-window-handle's
    // `HasWindowHandle`: winit, tao, eframe, iced, Slint, SDL, GLFW, ...
    let browser = Browser::builder()
        .engine(Engine::WebKit) // or leave out for the default engine
        .profile(&profile)
        .url("https://www.rust-lang.org")
        .on_event(|event| println!("{event:?}"))
        .build(&window, Rect::new(0.0, 0.0, 1024.0, 768.0))?;

    // ... run the application's event loop ...

    drop(browser);
    polybrowser::shutdown();
    Ok(())
}
```

## Engines

Cargo features decide which engines are compiled in; each `Browser` picks one
of them at run time, so an application can ship one engine or offer several.

| Feature    | Engine                                              | Default | Cost                         |
|------------|-----------------------------------------------------|---------|------------------------------|
| `webkit`   | `Engine::WebKit`: WKWebView (macOS), WebKitGTK (Linux) | yes  | none, part of the system     |
| `webview2` | `Engine::WebView2`: Edge WebView2 (Windows)         | yes     | none, part of the system     |
| `chromium` | `Engine::Chromium`: Chromium via CEF                | no      | ~320 MB shipped with the app |

`polybrowser::engines()` lists what a build contains. With
`default-features = false` and no engine feature, the crate is only the data
layer (profiles, history, bookmarks, import, export) and pulls in no web view.

### What has been run, and where

| Platform | WebKit / WebView2                     | Chromium                  |
|----------|---------------------------------------|---------------------------|
| macOS    | works, covered by the self-test       | works, covered by the self-test |
| Linux    | builds on Ubuntu 24.04 ARM64; native browser operations exercised in Muxy engine tests | not implemented |
| Windows  | written, never compiled or run        | not implemented           |

The system web views go through [wry](https://github.com/tauri-apps/wry), which
supports all three platforms, and the platform-specific code here is small.
The native host integration has also been tested on Ubuntu 24.04 ARM64. On Linux wry needs GTK
initialised and an X11 window. Chromium needs platform code for the child
window and the helper processes; only the macOS half exists.

## Profiles

A profile has its own cookies and site data in every engine, and one history
and one set of bookmarks shared between engines.

```rust
let profiles = ProfileManager::open("com.example.myapp")?; // or open_at(path)
let work = profiles.create("Work")?;
let all = profiles.list()?;
profiles.rename(work.id(), "Office")?;
profiles.delete(work.id())?;

let private = profiles.ephemeral(); // nothing written to disk
```

```text
<root>/profiles/<id>/profile.json      name and creation time
<root>/profiles/<id>/library.sqlite    history and bookmarks
<root>/engines/<engine>/<id>/          that engine's cookies, cache, storage
```

WKWebView cannot be told where to store data, so on macOS WebKit keeps each
profile in a data store of its own under `~/Library/WebKit` (this needs macOS
14; older systems share one store). Deleting a profile erases that store the
next time WebKit is used.

## History, import, export

Pages opened in a `Browser` are recorded in its profile's history, including
address changes made with `history.pushState`.

```rust
use polybrowser::{import, HistoryFormat, HistoryQuery, ImportOptions};

let history = profile.history()?;
let recent = history.entries(&HistoryQuery::new().text("rust").limit(20))?;

// Bring in history and bookmarks from the browsers on this machine.
for source in import::detect() {
    let report = profile.import_from(&source, &ImportOptions::default())?;
    println!("{}: {} new visits", source.browser(), report.visits_added);
}

history.export_to_file(HistoryFormat::Json, "history.json")?; // also Csv, Html
profile.bookmarks()?.export_html(std::fs::File::create("bookmarks.html")?)?;
```

Import reads Chrome, Chromium, Edge, Brave, Vivaldi, Opera, Arc, Firefox and
Safari. It works on a temporary copy of the browser's files, so the browser
can be running. Importing twice adds nothing twice. Cookies, passwords and open
tabs are not imported. Safari's data is protected by macOS: the application
needs Full Disk Access, and gets `Error::PermissionDenied` without it.

The importers are tested against databases built to each browser's schema, not
yet against real browser profiles.

## Scripts, snapshots, find and cookies

A `Browser` exposes the parts of a page an application usually needs. Each runs
on the main thread and reports its result to a callback, also on the main
thread:

```rust
browser.run_script("return document.title;", |result| println!("{result:?}"))?;
browser.snapshot(|image| { /* RGBA, works even while the view is hidden */ })?;
browser.find("needle", FindOptions::default(), |found| println!("{found:?}"))?;
browser.cookies(None, |cookies| { /* the profile's cookies */ })?;
browser.set_cookie(cookie, |done| { /* ... */ })?;
```

`clear_cookies`, `delete_cookies` and `clear_find` round these out. The same
cookie and site-data operations also work without a view, given only a profile:
`polybrowser::clear_site_data(engine, &profile, done)` and
`polybrowser::import_cookies(engine, &profile, cookies, done)`.

Besides `LoadStarted`, `LoadFinished`, `UrlChanged` and `TitleChanged`, a view
reports `LoadFailed`, `ProgressChanged`, `NavigationStateChanged`,
`FaviconChanged`, `CloseRequested` (the page called `window.close()`) and
`ExternalLinkRequested` (the user clicked a `mailto:`, `tel:` or similar link
the engine cannot open, for the application to hand to the OS).

Scripts, snapshots, find and cookie operations are implemented for WebKit on macOS
and Linux (WebKitGTK 4.1, version 2.42+). The extra native event observers above
remain macOS-specific; a custom Linux host can provide its own observers. `ExternalProfile::read_cookies`
also reads cookies from the installed browsers, decrypting the Chromium family's
with the key from the macOS login keychain.

## Developer tools

Every engine's page inspector is available: Web Inspector in WebKit, DevTools
in Chromium and WebView2.

```rust
let browser = Browser::builder().devtools(true).build(&window, bounds)?;
browser.open_devtools()?;
assert!(browser.is_devtools_open());
browser.close_devtools()?;
```

`devtools` is on by default in debug builds and off in release builds, where
`open_devtools` then returns an error. To keep the inspector in a release
build, pass `devtools(true)`, and for the system web views also enable the
`devtools` Cargo feature. On macOS that feature uses a private WebKit API,
which the App Store does not accept. Chromium needs no feature.

In the system web views the inspector can also be opened from the page's
context menu (Inspect Element). Chromium opens it in a window of its own.

## Examples

```sh
cargo run -p polybrowser --example minimal -- webkit https://example.com
cargo run -p polybrowser --example selftest          # end-to-end check of an engine
cargo run -p polybrowser --example import_history    # list importable browsers
cargo run -p polybrowser --example import_history -- ./out   # import and export
```

`selftest` opens a window for a few seconds, browses pages from a local server
and checks events, history, that cookies stay within their profile, and that
the inspector opens and closes.

## Chromium

Building with `--features chromium` downloads a CEF distribution (about 1 GB
unpacked, into `target/cef`, see `.cargo/config.toml`) and compiles its C++
glue, which needs **CMake** and **Ninja** (`brew install cmake ninja`).

Chromium on macOS only runs from an application bundle that contains the
framework and five helper applications. `polybrowser-bundle` builds one and,
with `--run`, starts it. The `chromium` example is a Chromium window:

```sh
cargo run -p polybrowser-bundler -- \
    --package polybrowser --example chromium --features chromium \
    --run -- --use-mock-keychain
```

After the last `--` it takes a URL (it shows `chrome://version` without one),
`--profile NAME` to keep cookies and site data in a profile of that name
instead of browsing privately, and `--devtools` to open DevTools as well.

The self-test runs the same way, with `--example selftest` and
`--run -- chromium --use-mock-keychain`.

Use `--bin NAME` for your own application. The bundle lands next to the
executable, in `target/<profile>/bundle/`. It is ad-hoc signed, which is enough
to run it locally; to distribute it, sign every bundle inside with your own
identity.

Things to know:

- **`init` and `shutdown`.** Call `polybrowser::init()` first in `main` and
  `polybrowser::shutdown()` after the last `Browser` is dropped. Without a
  clean shutdown Chromium can lose the last seconds of cookie changes.
- **One data root per process.** All profiles used with Chromium in one process
  must come from the same `ProfileManager`, private ones included
  (`profiles.ephemeral()`), and only one process can use that root at a time.
- **Keychain.** Chromium encrypts cookies with a key in the macOS keychain and
  asks for access the first time. `--use-mock-keychain` on the application's
  command line avoids the prompt during development, at the price of a fixed
  key.
- **Not done yet:** `user_agent` is ignored, and `clear_browsing_data` clears
  cookies and the HTTP cache but not `localStorage` or IndexedDB.

## Crates

| Crate                    | What it is                                                        |
|--------------------------|-------------------------------------------------------------------|
| `polybrowser`            | What applications depend on: `Browser`, plus everything below     |
| `polybrowser-core`       | Profiles, history, bookmarks, import/export, the backend traits   |
| `polybrowser-wry`        | WebKit and WebView2 backend                                       |
| `polybrowser-cef`        | Chromium backend                                                  |
| `polybrowser-cef-helper` | The executable Chromium runs its child processes in               |
| `polybrowser-bundler`    | `polybrowser-bundle`, the macOS bundling tool                     |

An engine is an implementation of `polybrowser::backend::EngineBackend`.
`Browser::builder().backend(...)` accepts one from outside this workspace.

## Notes for embedders

- Views belong to the main thread, and do not follow their window: call
  `Browser::set_bounds` when the window is resized.
- On macOS, a bundled application that shows WebKit views needs
  `NSAllowsArbitraryLoadsInWebContent` in its `Info.plist` to open `http://`
  pages, and an Edit menu for copy and paste shortcuts to reach the page.
- By default a link that wants a new window loads in the same view. Set
  `NewWindowPolicy::Block` and handle `BrowserEvent::NewWindowRequested` to
  open a tab instead.

## A custom Linux host

Enable `webkit-dynamic` with `default-features = false` to use
`polybrowser::webkitgtk::{NativeView, SiteData}` with a native GTK host. These
operations load WebKitGTK and its supporting libraries at runtime, so applications
can still start without the engine installed. They do not initialize GTK, create
views, or drive its event loop. The host retains responsibility for those tasks,
input, rendering, navigation policy and browser events. `from_raw` retains the
native object; all use and drop must stay on its GTK thread.

The Wry Linux backend uses these same operations. Scripts and snapshots are
cancelled on document navigation or view destruction, limited to 128 pending
requests per view, and time out after 60 seconds. Snapshots temporarily map hidden
views in a GTK offscreen window and return straight RGBA pixels. Closing the last
view releases its shared persistent profile context.
