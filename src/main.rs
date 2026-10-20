//! Native file browser and the same scene rendered offscreen for evidence.
#![forbid(unsafe_code)]

use browser_ui::{BrowserView, make_ui};
use mui::host::View;
use mui::prelude::{Input, Size};
use std::ffi::OsString;
use std::path::PathBuf;

struct Options {
    path: PathBuf,
    /// Further locations, each opened in its own tab after the first.
    more: Vec<PathBuf>,
    fixture: Option<usize>,
    snapshot: Option<PathBuf>,
    size: (u32, u32),
    profile: Option<PathBuf>,
}

fn value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let (width, height) = value.split_once('x').ok_or("size must be WIDTHxHEIGHT")?;
    let width: u32 = width.parse().map_err(|_| "invalid width")?;
    let height: u32 = height.parse().map_err(|_| "invalid height")?;
    if !(320..=4096).contains(&width) || !(240..=4096).contains(&height) {
        return Err("size must be between 320x240 and 4096x4096".into());
    }
    Ok((width, height))
}

fn parse() -> Result<Option<Options>, String> {
    let mut options = Options {
        path: std::env::current_dir().map_err(|e| e.to_string())?,
        more: Vec::new(),
        fixture: None,
        snapshot: None,
        size: (1200, 800),
        profile: None,
    };
    let mut args = std::env::args_os().skip(1);
    let mut path_seen = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "megaman [PATH...] [--fixture COUNT] [--size WIDTHxHEIGHT] [--profile CSV]\n\nNative file browser. Each PATH opens in its own tab. Linux X11 Ctrl+C/Ctrl+V copies files with bounded verified operations. Ctrl+Shift+F opens Find Anywhere with an explicit folder scope.\n--fixture uses labeled synthetic entries; --snapshot PNG requires --fixture and uses the CPU path.\nExperimental build; desktop integrations and additional file operations are in development."
                );
                return Ok(None);
            }
            Some("--version") => {
                println!("megaman {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Some("--fixture") => {
                let count = value(&mut args, "--fixture")?;
                let count: usize = count
                    .to_str()
                    .ok_or("invalid fixture count")?
                    .parse()
                    .map_err(|_| "invalid fixture count")?;
                if count > 10_000_000 {
                    return Err("fixture count must not exceed 10000000".into());
                }
                options.fixture = Some(count);
            }
            Some("--snapshot") => options.snapshot = Some(value(&mut args, "--snapshot")?.into()),
            Some("--profile") => options.profile = Some(value(&mut args, "--profile")?.into()),
            Some("--size") => {
                options.size =
                    parse_size(value(&mut args, "--size")?.to_str().ok_or("invalid size")?)?
            }
            Some("--") => {
                for arg in args.by_ref() {
                    if path_seen {
                        options.more.push(arg.into());
                    } else {
                        options.path = arg.into();
                        path_seen = true;
                    }
                }
                break;
            }
            Some(flag) if flag.starts_with('-') => return Err(format!("unknown option: {flag}")),
            _ => {
                if path_seen {
                    options.more.push(arg.into());
                } else {
                    options.path = arg.into();
                    path_seen = true;
                }
            }
        }
    }
    if options.snapshot.is_some() && options.fixture.is_none() {
        return Err(
            "--snapshot requires --fixture COUNT for reproducible synthetic evidence".into(),
        );
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    for path in std::iter::once(&mut options.path).chain(options.more.iter_mut()) {
        if path.is_relative() {
            *path = cwd.join(&*path);
        }
    }
    Ok(Some(options))
}

fn snapshot(
    view: &mut BrowserView,
    options: &Options,
    path: &std::path::Path,
) -> Result<(), String> {
    use mui::vello::vello_cpu::{Pixmap, RenderContext, Resources};
    let (width, height) = options.size;
    let mut ui = make_ui()?;
    let input = Input::default();
    // Same retained interaction/layout pipeline as the native host. Settle initial layout.
    for pass in 0..2 {
        let root = view.build(&mut ui, &input);
        ui.frame(
            root,
            Some(Size::new(width as f64, height as f64)),
            Input::default(),
            1.0,
        )
        .map_err(|e| format!("layout settle pass {pass}: {e}; {}", view.evidence()))?;
        view.after_frame(&mut ui);
    }
    let root = view.build(&mut ui, &input);
    let frame = ui
        .frame(
            root,
            Some(Size::new(width as f64, height as f64)),
            Input::default(),
            1.0,
        )
        .map_err(|e| e.to_string())?;
    let mut context = RenderContext::new(width as u16, height as u16);
    let mut resources = Resources::default();
    mui::vello::paint(
        &mut mui::vello::Cpu {
            ctx: &mut context,
            resources: &mut resources,
            cache: &mut mui::vello::Cache::default(),
        },
        frame.scene,
        mui::vello::kurbo::Affine::IDENTITY,
    )
    .map_err(|e| e.to_string())?;
    context.flush();
    let mut pixmap = Pixmap::new(width as u16, height as u16);
    context.render(&mut pixmap, &mut resources);
    let pixels = pixmap.take_rgba8(mui::vello::peniko::ImageAlphaType::Alpha);
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .map_err(|e| e.to_string())?
        .write_image_data(&pixels)
        .map_err(|e| e.to_string())?;
    println!("{}", view.evidence());
    Ok(())
}

fn run() -> Result<(), String> {
    let Some(options) = parse()? else {
        return Ok(());
    };
    let mut view = options.fixture.map_or_else(
        || {
            let mut view = BrowserView::new(options.path.clone());
            for path in &options.more {
                view.open_tab(path.clone());
            }
            view
        },
        BrowserView::fixture,
    );
    if let Some(path) = &options.snapshot {
        return snapshot(&mut view, &options, path);
    }
    #[cfg(feature = "desktop")]
    {
        #[cfg(target_os = "linux")]
        if options.fixture.is_none() {
            view.enable_native_file_clipboard();
        }
        let shared = std::sync::Arc::new(std::sync::Mutex::new(mui_winit::Shared {
            view,
            ui: make_ui()?,
        }));
        mui_winit::run_shared(
            std::sync::Arc::clone(&shared),
            mui_winit::Options {
                title: "MegaMan".into(),
                size: options.size,
                profile_output: options.profile,
                ..Default::default()
            },
        )?;
        println!("{}", mui_winit::lock(&shared).view.evidence());
        Ok(())
    }
    #[cfg(not(feature = "desktop"))]
    Err("this build has no desktop host; use --fixture COUNT --snapshot PATH".into())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("megaman: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_size;
    #[test]
    fn reject_invalid_or_unbounded_surface_allocations() {
        for invalid in [
            "0x480",
            "-1x480",
            "640X480",
            "640x",
            "640x500000",
            "NaNx480",
            "640x480x2",
        ] {
            assert!(parse_size(invalid).is_err(), "{invalid}");
        }
        assert_eq!(parse_size("640x480").unwrap(), (640, 480));
    }
}
