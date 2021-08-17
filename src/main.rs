use anyhow::{Context, Result};
use image::io::Reader as ImageReader;
use image::RgbImage;
use std::path::PathBuf;
use std::process::Command;
use structopt::StructOpt;
use x11rb::{
    connection::Connection,
    image as x11image,
    protocol::xproto::{
        ConnectionExt, CreateGCAux, CreateWindowAux, Screen, VisualClass, Visualid, Window,
        WindowClass,
    },
};

#[derive(StructOpt)]
struct Opt {
    /// Path to image file
    #[structopt(parse(from_os_str))]
    image_file: PathBuf,

    /// Terminal row to display the image in
    #[structopt()]
    row: i16,

    /// Terminal column to display the image in
    #[structopt()]
    col: i16,
}

/// Taken from https://github.com/psychon/x11rb/blob/84a877d72b87ac4de82aa77c4cfc0598ed41732a/examples/display_ppm.rs#L73-L107
/// Check that the given visual is "as expected" (pixel values are 0xRRGGBB with RR/GG/BB being the
/// colors). Otherwise, this exits the process.
fn check_visual(screen: &Screen, id: Visualid) -> Result<x11image::PixelLayout> {
    // Find the information about the visual and at the same time check its depth.
    let visual_info = screen
        .allowed_depths
        .iter()
        .filter_map(|depth| {
            let info = depth.visuals.iter().find(|depth| depth.visual_id == id);
            info.map(|info| (depth.depth, info))
        })
        .next();
    let (depth, visual_type) = match visual_info {
        Some(info) => info,
        None => {
            eprintln!("Did not find the root visual's description?!");
            std::process::exit(1);
        }
    };
    // Check that the pixels have red/green/blue components that we can set directly.
    match visual_type.class {
        VisualClass::TRUE_COLOR | VisualClass::DIRECT_COLOR => {}
        _ => {
            eprintln!(
                "The root visual is not true / direct color, but {:?}",
                visual_type,
            );
            std::process::exit(1);
        }
    }
    let result = x11image::PixelLayout::from_visual_type(*visual_type)
        .with_context(|| "The server sent a malformed visual type")?;
    assert_eq!(result.depth(), depth);
    Ok(result)
}

fn get_current_window_id() -> Result<Window> {
    let output = Command::new("xdotool")
        .arg("getwindowfocus")
        .output()
        .with_context(|| "Failed to run xdotool")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // TODO check exit code
    stdout
        .trim()
        .parse()
        .with_context(|| "Couldn't parse window ID number from xdotool")
}

fn rowcol_to_pixels(
    conn: &impl Connection,
    window: Window,
    (row, col): (i16, i16),
) -> Result<(i16, i16)> {
    // Get geometry of the given window
    let window_geometry = conn.get_geometry(window)?.reply()?;
    dbg!(window_geometry);

    let (cols, rows) = termion::terminal_size().with_context(|| "Could not get terminal size")?;
    let (xpixels, ypixels) =
        termion::terminal_size_pixels().with_context(|| "Could not get terminal size in pixels")?;
    let pixels_per_row = (ypixels / rows) as i16;
    let pixels_per_col = (xpixels / cols) as i16;
    let yoffset = ((window_geometry.height - ypixels) as i16) / 2;
    let xoffset = ((window_geometry.width - xpixels) as i16) / 2;

    Ok((
        xoffset + col * pixels_per_col,
        yoffset + row * pixels_per_row,
    ))
}

fn show_image(image: RgbImage, window: Window, (x, y): (i16, i16)) -> Result<()> {
    // Connect to the X server
    let (conn, screen_num) = x11rb::connect(None).with_context(|| "Couldn't connect to X")?;
    let screen = &conn.setup().roots[screen_num];

    // Convert (x, y) to pixels
    let (x, y) = rowcol_to_pixels(&conn, window, (x, y))?;

    // Get image information and create x11rb image
    let (w, h) = image.dimensions();
    let w = w as u16;
    let h = h as u16;
    let img = x11image::Image::new(
        w,
        h,
        x11image::ScanlinePad::Pad8,
        24,
        x11image::BitsPerPixel::B24,
        x11image::ImageOrder::MSBFirst,
        image.into_raw().into(),
    )?;

    // Change x11rb to use the appropriate format
    let img_layout = x11image::PixelLayout::new(
        x11image::ColorComponent::new(8, 16)?,
        x11image::ColorComponent::new(8, 8)?,
        x11image::ColorComponent::new(8, 0)?,
    );
    let pixel_layout = check_visual(screen, screen.root_visual)?;
    let img = img.reencode(img_layout, pixel_layout, conn.setup())?;

    // Create graphics context
    let gc_id = conn.generate_id()?;
    conn.create_gc(
        gc_id,
        screen.root,
        &CreateGCAux::new().graphics_exposures(0),
    )?;
    // Create and paint pixmap
    let pixmap_id = conn.generate_id()?;
    conn.create_pixmap(screen.root_depth, pixmap_id, screen.root, w, h)?;
    img.put(&conn, pixmap_id, gc_id, 0, 0)?;
    // Create window
    let win_id = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        win_id,
        screen.root, // current_window_id,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::default().background_pixmap(pixmap_id),
    )?;
    conn.reparent_window(win_id, window, x, y)?;

    // Free pixmap&gcontext
    conn.free_pixmap(pixmap_id)?;
    conn.free_gc(gc_id)?;

    // Map the window
    conn.map_window(win_id)?;

    // Flush the connection
    conn.flush()?;

    // TODO: rework this loop
    loop {
        println!("Event: {:?}", conn.wait_for_event()?);
    }
}

fn main() -> Result<()> {
    let opt: Opt = Opt::from_args();

    let image: RgbImage = ImageReader::open(opt.image_file)?.decode()?.to_rgb8();

    let window = get_current_window_id()?;

    show_image(image, window, (opt.row, opt.col))?;

    Ok(())
}
