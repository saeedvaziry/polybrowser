//! Puts a Chromium-enabled application into the `.app` bundle macOS needs.
//!
//! Chromium on macOS only runs from an application bundle laid out like this:
//!
//! ```text
//! My App.app/Contents/
//!   Info.plist
//!   MacOS/myapp
//!   Frameworks/
//!     Chromium Embedded Framework.framework/
//!     myapp Helper.app/                 one helper bundle per kind of
//!     myapp Helper (GPU).app/           child process, each wrapping the
//!     myapp Helper (Renderer).app/      same helper executable
//!     myapp Helper (Plugin).app/
//!     myapp Helper (Alerts).app/
//! ```
//!
//! This tool builds the application and the helper with Cargo and assembles
//! that. The result is for development: it is only ad-hoc signed. To
//! distribute it, sign every bundle in it with your own identity.

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::Value;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "\
Usage: polybrowser-bundle (--bin NAME | --example NAME) [options] [-- ARGS...]

Builds NAME with Cargo and wraps it in a macOS application bundle together
with Chromium and its helper processes.

Options:
  -p, --package PKG     Package that contains the target
      --features LIST   Features to build the target with (add `chromium`)
      --release         Build in release mode
      --name NAME       Name of the application (default: the target's name)
      --identifier ID   Bundle identifier (default: dev.polybrowser.<target>)
      --helper-bin BIN  Use a binary of the same package as the helper, instead
                        of the polybrowser-cef-helper package
      --run             Start the application once it is bundled
  ARGS                  Arguments for the application, with --run
";

const FRAMEWORK: &str = "Chromium Embedded Framework.framework";
const HELPERS: [&str; 5] = ["", " (GPU)", " (Renderer)", " (Plugin)", " (Alerts)"];

#[derive(Default)]
struct Options {
    /// `--bin` or `--example`, and the target's name.
    target: Option<(&'static str, String)>,
    package: Option<String>,
    features: Option<String>,
    release: bool,
    name: Option<String>,
    identifier: Option<String>,
    helper_bin: Option<String>,
    run: bool,
    app_args: Vec<String>,
}

fn parse_args() -> Result<Options> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |what: &str| args.next().ok_or(format!("{arg} needs {what}"));
        match arg.as_str() {
            "--bin" => options.target = Some(("--bin", value("a name")?)),
            "--example" => options.target = Some(("--example", value("a name")?)),
            "-p" | "--package" => options.package = Some(value("a package")?),
            "--features" => options.features = Some(value("a list")?),
            "--name" => options.name = Some(value("a name")?),
            "--identifier" => options.identifier = Some(value("an identifier")?),
            "--helper-bin" => options.helper_bin = Some(value("a binary name")?),
            "--release" => options.release = true,
            "--run" => options.run = true,
            "--" => {
                options.app_args = args.collect();
                break;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}").into()),
        }
    }
    if options.target.is_none() {
        return Err(format!("say what to bundle with --bin or --example\n\n{USAGE}").into());
    }
    Ok(options)
}

/// What a `cargo build` produced that the bundle needs.
struct Built {
    executable: PathBuf,
    /// Where the build put the CEF distribution, if it built CEF.
    cef_dir: Option<PathBuf>,
}

/// Runs `cargo build` and picks the executable called `name`, and the
/// location of CEF, out of Cargo's own account of the build.
fn cargo_build(selection: &[&str], name: &str, release: bool) -> Result<Built> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut command = Command::new(cargo);
    command
        .arg("build")
        .args(selection)
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped());
    if release {
        command.arg("--release");
    }
    let output = command.spawn()?.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("building {name} failed").into());
    }

    let (mut executable, mut cef_dir) = (None, None);
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match message["reason"].as_str() {
            Some("compiler-artifact") if message["target"]["name"] == name => {
                if let Some(path) = message["executable"].as_str() {
                    executable = Some(PathBuf::from(path));
                }
            }
            // CEF's build script adds the distribution to the link search path.
            Some("build-script-executed") => {
                for path in message["linked_paths"].as_array().into_iter().flatten() {
                    let path = path.as_str().unwrap_or_default();
                    let path = Path::new(path.strip_prefix("native=").unwrap_or(path));
                    if path.join(FRAMEWORK).is_dir() {
                        cef_dir = Some(path.to_owned());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Built {
        executable: executable.ok_or(format!("Cargo built no executable called {name}"))?,
        cef_dir,
    })
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn info_plist(executable: &str, name: &str, identifier: &str, helper: bool) -> String {
    let (executable, name, identifier) = (
        xml_escape(executable),
        xml_escape(name),
        xml_escape(identifier),
    );
    // Helpers must not show up in the Dock.
    let kind = if helper {
        "    <key>LSUIElement</key>\n    <string>1</string>\n"
    } else {
        // A browser has to be able to open http:// pages. Chromium does not
        // consult this setting, but a WKWebView in the same application does.
        "    <key>NSPrincipalClass</key>\n    <string>NSApplication</string>\n\
         \x20   <key>NSAppTransportSecurity</key>\n    <dict>\n\
         \x20       <key>NSAllowsArbitraryLoadsInWebContent</key>\n        <true/>\n    </dict>\n"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>{executable}</string>
    <key>CFBundleIdentifier</key>
    <string>{identifier}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundleDisplayName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
{kind}</dict>
</plist>
"#,
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// Writes `<parent>/<bundle name>.app` around a copy of `binary`.
fn write_app(
    parent: &Path,
    bundle_name: &str,
    executable_name: &str,
    identifier: &str,
    helper: bool,
    binary: &Path,
) -> Result<PathBuf> {
    let app = parent.join(format!("{bundle_name}.app"));
    let contents = app.join("Contents");
    fs::create_dir_all(contents.join("MacOS"))?;
    fs::write(
        contents.join("Info.plist"),
        info_plist(executable_name, bundle_name, identifier, helper),
    )?;
    let target = contents.join("MacOS").join(executable_name);
    // Replace rather than overwrite: the running kernel caches the signature
    // of an executable by inode.
    let _ = fs::remove_file(&target);
    fs::copy(binary, &target)?;
    Ok(app)
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&source, &target)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&source)?, &target)?;
        } else {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// Copies Chromium into the bundle unless an identical copy is there already.
fn install_framework(cef_dir: &Path, frameworks: &Path) -> Result<()> {
    let (source, target) = (cef_dir.join(FRAMEWORK), frameworks.join(FRAMEWORK));
    let binary = "Chromium Embedded Framework";
    let size = |path: &Path| fs::metadata(path.join(binary)).map(|meta| meta.len()).ok();
    if size(&target).is_some() && size(&target) == size(&source) {
        return Ok(());
    }
    if target.exists() {
        fs::remove_dir_all(&target)?;
    }
    Ok(copy_dir(&source, &target)?)
}

fn run() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(
            "application bundles are a macOS matter; nothing to do on this platform".into(),
        );
    }
    let options = parse_args()?;
    let (kind, target) = options.target.clone().expect("checked while parsing");

    let mut selection = vec![kind, target.as_str()];
    if let Some(package) = &options.package {
        selection.extend(["--package", package]);
    }
    if let Some(features) = &options.features {
        selection.extend(["--features", features]);
    }
    let app = cargo_build(&selection, &target, options.release)?;
    let cef_dir = app.cef_dir.ok_or(
        "the build did not include Chromium; build with polybrowser's `chromium` feature \
         (for example `--features chromium`)",
    )?;

    let helper = match &options.helper_bin {
        Some(bin) => {
            let mut selection = vec!["--bin", bin.as_str()];
            if let Some(package) = &options.package {
                selection.extend(["--package", package]);
            }
            if let Some(features) = &options.features {
                selection.extend(["--features", features]);
            }
            cargo_build(&selection, bin, options.release)?
        }
        None => cargo_build(
            &["--package", "polybrowser-cef-helper"],
            "polybrowser-cef-helper",
            options.release,
        )?,
    };

    let name = options.name.clone().unwrap_or_else(|| target.clone());
    let identifier = options
        .identifier
        .clone()
        .unwrap_or_else(|| format!("dev.polybrowser.{}", target.replace('_', "-")));
    // Next to the executable Cargo built: target/<profile>[/examples]/bundle.
    let out_dir = app
        .executable
        .parent()
        .unwrap_or(Path::new("."))
        .join("bundle");
    fs::create_dir_all(&out_dir)?;

    let bundle = write_app(
        &out_dir,
        &name,
        &target,
        &identifier,
        false,
        &app.executable,
    )?;
    let frameworks = bundle.join("Contents/Frameworks");
    fs::create_dir_all(&frameworks)?;
    install_framework(&cef_dir, &frameworks)?;
    // Chromium looks its helpers up by the name of the main executable.
    for suffix in HELPERS {
        let helper_name = format!("{target} Helper{suffix}");
        write_app(
            &frameworks,
            &helper_name,
            &helper_name,
            &format!("{identifier}.helper"),
            true,
            &helper.executable,
        )?;
    }
    println!("Bundled {}", bundle.display());

    if options.run {
        let status = Command::new(bundle.join("Contents/MacOS").join(&target))
            .args(&options.app_args)
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("polybrowser-bundle: {error}");
        std::process::exit(1);
    }
}
