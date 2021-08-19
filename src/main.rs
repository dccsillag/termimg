use anyhow::{Context, Error, Result};
use image::io::Reader as ImageReader;
use image::RgbImage;
use std::borrow::Cow;
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
    if output.status.success() {
        stdout
            .trim()
            .parse()
            .with_context(|| "Couldn't parse window ID number from xdotool")
    } else {
        Err(Error::msg(String::from_utf8(output.stderr)?))
            .with_context(|| "xdotool exited with non-zero status")
    }
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

struct ImageDisplay<'a> {
    image: Cow<'a, x11image::Image<'a>>,
    parent_window: Window,

    window: Option<Window>,
}

impl<'a> ImageDisplay<'a> {
    fn new(
        conn: &impl Connection,
        screen: &Screen,
        image: RgbImage,
        parent_window: Window,
    ) -> Result<Self> {
        // Get image information and create x11rb image
        let (w, h) = image.dimensions();
        let w = w as u16;
        let h = h as u16;

        // Change x11rb image to use the appropriate format
        let img_layout = x11image::PixelLayout::new(
            x11image::ColorComponent::new(8, 16)?,
            x11image::ColorComponent::new(8, 8)?,
            x11image::ColorComponent::new(8, 0)?,
        );
        let pixel_layout = check_visual(screen, screen.root_visual)?;
        let img = x11image::Image::new(
            w,
            h,
            x11image::ScanlinePad::Pad8,
            24,
            x11image::BitsPerPixel::B24,
            x11image::ImageOrder::MSBFirst,
            image.into_raw().into(),
        )?;
        let img = img
            .reencode(img_layout, pixel_layout, conn.setup())?
            .into_owned();

        Ok(Self {
            image: Cow::Owned(img),
            parent_window,
            window: None,
        })
    }

    fn is_shown(&self) -> bool {
        self.window.is_some()
    }

    fn remove(&mut self, conn: &impl Connection) -> Result<()> {
        assert!(self.is_shown());
        let window = self.window.unwrap();

        conn.unmap_window(window)?;
        self.window = None;

        Ok(())
    }

    fn show_at(
        &mut self,
        conn: &impl Connection,
        screen: &Screen,
        (x, y): (i16, i16),
    ) -> Result<()> {
        if self.is_shown() {
            self.remove(conn)?;
        }
        assert!(!self.is_shown());

        // Create graphics context
        let gc_id = conn.generate_id()?;
        conn.create_gc(
            gc_id,
            screen.root,
            &CreateGCAux::new().graphics_exposures(0),
        )?;
        // Create and paint pixmap
        let pixmap_id = conn.generate_id()?;
        conn.create_pixmap(
            screen.root_depth,
            pixmap_id,
            screen.root,
            self.image.width(),
            self.image.height(),
        )?;
        self.image.put(conn, pixmap_id, gc_id, 0, 0)?;
        // Create window
        let win_id = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            win_id,
            screen.root,
            0,
            0,
            self.image.width(),
            self.image.height(),
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::default().background_pixmap(pixmap_id),
        )?;
        conn.reparent_window(win_id, self.parent_window, x, y)?;

        // Free pixmap&gcontext
        conn.free_pixmap(pixmap_id)?;
        conn.free_gc(gc_id)?;

        // Map the window
        conn.map_window(win_id)?;

        // Flush the connection
        conn.flush()?;

        // Set fields
        self.window = Some(win_id);

        Ok(())
    }

    fn tick(&mut self, conn: &impl Connection) -> Result<()> {
        // TODO
        println!("Event: {:?}", conn.wait_for_event()?);

        Ok(())
    }
}

fn main() -> Result<()> {
    let opt: Opt = Opt::from_args();

    // Connect to the X server
    let (conn, screen_num) = x11rb::connect(None).with_context(|| "Couldn't connect to X")?;
    let screen = &conn.setup().roots[screen_num];

    // Get current window
    let window = get_current_window_id()?;

    // Convert (x, y) to pixels
    let (x, y) = rowcol_to_pixels(&conn, window, (opt.col, opt.row))?;

    // Load the image
    let image: RgbImage = ImageReader::open(opt.image_file)?.decode()?.to_rgb8();

    // Show the image
    let mut display_image = ImageDisplay::new(&conn, screen, image, window)?;
    display_image.show_at(&conn, screen, (x, y))?;

    // Handle it
    loop {
        display_image.tick(&conn)?;
    }
}
