//! Chromium does not render pages in the application's own process. On macOS
//! it starts copies of this executable instead, from helper bundles inside
//! the application bundle. `polybrowser-bundle` puts them there.

fn main() {
    #[cfg(target_os = "macos")]
    std::process::exit(polybrowser_cef::run_helper());

    #[cfg(not(target_os = "macos"))]
    eprintln!("polybrowser-cef-helper is only used on macOS");
}
