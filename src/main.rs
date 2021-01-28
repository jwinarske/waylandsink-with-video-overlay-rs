extern crate gstreamer as gst;
extern crate gstreamer_app as gst_app;
extern crate gstreamer_video as gst_video;
extern crate wayland_sys;

use std::{cmp::min, io::Write, os::unix::io::AsRawFd};
use std::ffi::c_void;
use std::process::exit;

use anyhow::Error;
use derive_more::{Display, Error};
use gst::prelude::*;
use gst_video::prelude::*;
use wayland_client::{Display, event_enum, EventQueue, Filter, GlobalManager, Main,
                     protocol::{wl_compositor, wl_keyboard, wl_pointer, wl_seat, wl_shm}};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_protocols::xdg_shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Debug, Display, Error)]
#[display(fmt = "Missing element {}", _0)]
struct MissingElement(#[error(not(source))] &'static str);

#[derive(Debug, Display, Error)]
#[display(fmt = "Received error from {}: {} (debug: {:?})", src, error, debug)]
struct ErrorMessage {
    src: String,
    error: String,
    debug: Option<String>,
    source: gst::glib::Error,
}

// declare an event enum containing the events we want to receive in the iterator
event_enum!(
    Events |
    Pointer => wl_pointer::WlPointer,
    Keyboard => wl_keyboard::WlKeyboard
);

const WIDTH: usize = 320;
const HEIGHT: usize = 240;
const GST_WAYLAND_DISPLAY_HANDLE_CONTEXT_TYPE: &str = "GstWaylandDisplayHandleContextType";

fn create_pipeline(surface: Main<WlSurface>, display: Display, event_queue: &mut EventQueue) -> Result<(gst::Pipeline, &mut EventQueue), Error> {
    gst::init()?;

    let pipeline = gst::Pipeline::new(None);

    let src = gst::ElementFactory::make("appsrc", None)
        .map_err(|_| MissingElement("appsrc"))?;
    let videoconvert = gst::ElementFactory::make("videoconvert", None)
        .map_err(|_| MissingElement("videoconvert"))?;
    let sink = gst::ElementFactory::make("waylandsink", None)
        .map_err(|_| MissingElement("waylandsink"))?;

    let mut context = gst::Context::new(GST_WAYLAND_DISPLAY_HANDLE_CONTEXT_TYPE, true);
    {
        let context = context.get_mut().unwrap();
        let s = context.get_mut_structure();
        #[allow(clippy::cast_ptr_alignment)]
            let value = unsafe {
            use gst::glib::translate::*;
            use std::mem;

            let handle = display.c_ptr();
            let mut value = mem::MaybeUninit::zeroed();
            gst::gobject_sys::g_value_init(value.as_mut_ptr(), gst::gobject_sys::G_TYPE_POINTER);
            gst::gobject_sys::g_value_set_pointer(value.as_mut_ptr(), handle as *mut c_void);
            gst::glib::SendValue::from_glib_none(&value.assume_init() as *const _)
        };
        s.set_value("handle", value);
    }
    sink.set_context(&context);

    pipeline.add_many(&[&src, &videoconvert, &sink])?;
    gst::Element::link_many(&[&src, &videoconvert, &sink])?;


    let appsrc = src
        .dynamic_cast::<gst_app::AppSrc>()
        .expect("Source element is expected to be an appsrc!");

    // Specify the format we want to provide as application into the pipeline
    // by creating a video info with the given format and creating caps from it for the appsrc element.
    let video_info =
        gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgrx, WIDTH as u32, HEIGHT as u32)
            .fps(gst::Fraction::new(2, 1))
            .build()
            .expect("Failed to create video info");

    appsrc.set_caps(Some(&video_info.to_caps().unwrap()));
    appsrc.set_property_format(gst::Format::Time);

    // Our frame counter, that is stored in the mutable environment
    // of the closure of the need-data callback
    //
    // Alternatively we could also simply start a new thread that
    // pushes a buffer to the appsrc whenever it wants to, but this
    // is not really needed here. It is *not required* to use the
    // need-data callback.
    let mut i = 0;
    appsrc.set_callbacks(
        // Since our appsrc element operates in pull mode (it asks us to provide data),
        // we add a handler for the need-data callback and provide new data from there.
        // In our case, we told gstreamer that we do 2 frames per second. While the
        // buffers of all elements of the pipeline are still empty, this will be called
        // a couple of times until all of them are filled. After this initial period,
        // this handler will be called (on average) twice per second.
        gst_app::AppSrcCallbacks::builder()
            .need_data(move |appsrc, _| {
                // We only produce 50 frames
                if i == 50 {
                    let _ = appsrc.end_of_stream();
                    return;
                }

                println!("Producing frame {}", i);

                let r = if i % 2 == 0 { 0 } else { 255 };
                let g = if i % 3 == 0 { 0 } else { 255 };
                let b = if i % 5 == 0 { 0 } else { 255 };

                // Create the buffer that can hold exactly one BGRx frame.
                let mut buffer = gst::Buffer::with_size(video_info.size()).unwrap();
                {
                    let buffer = buffer.get_mut().unwrap();
                    // For each frame we produce, we set the timestamp when it should be displayed
                    // (pts = presentation time stamp)
                    // The autovideosink will use this information to display the frame at the right time.
                    buffer.set_pts(i * 500 * gst::MSECOND);

                    // At this point, buffer is only a reference to an existing memory region somewhere.
                    // When we want to access its content, we have to map it while requesting the required
                    // mode of access (read, read/write).
                    // See: https://gstreamer.freedesktop.org/documentation/plugin-development/advanced/allocation.html
                    let mut vframe =
                        gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &video_info)
                            .unwrap();

                    // Remember some values from the frame for later usage
                    let width = vframe.width() as usize;
                    let height = vframe.height() as usize;

                    // Each line of the first plane has this many bytes
                    let stride = vframe.plane_stride()[0] as usize;

                    // Iterate over each of the height many lines of length stride
                    for line in vframe
                        .plane_data_mut(0)
                        .unwrap()
                        .chunks_exact_mut(stride)
                        .take(height)
                    {
                        // Iterate over each pixel of 4 bytes in that line
                        for pixel in line[..(4 * width)].chunks_exact_mut(4) {
                            pixel[0] = b;
                            pixel[1] = g;
                            pixel[2] = r;
                            pixel[3] = 0;
                        }
                    }
                }

                i += 1;

                // appsrc already handles the error here
                let _ = appsrc.push_buffer(buffer);
            })
            .build(),
    );

    // Use the platform-specific sink to create our overlay.
    // Since we only use the video_overlay in the closure below, we need a weak reference.
    // !!ATTENTION!!:
    // It might seem appealing to use .clone() here, because that greatly
    // simplifies the code within the callback. What this actually does, however, is creating
    // a memory leak.
    let video_overlay = sink
        .dynamic_cast::<gst_video::VideoOverlay>()
        .unwrap()
        .downgrade();

    // Here we temporarily retrieve a strong reference on the video-overlay from the
    // weak reference that we moved into the closure.
    let video_overlay = video_overlay.upgrade().unwrap();

    #[allow(clippy::cast_ptr_alignment)]
        unsafe {
        // Here we ask native window handle we got assigned for
        // our video region from the window system, and then we will
        // pass this unique identifier to the overlay provided by our
        // sink - so the sink can then arrange the overlay.
        let native = surface.as_ref().c_ptr();
        video_overlay.set_window_handle(native as usize);
    }
    video_overlay.set_render_rectangle(0, 0, WIDTH as i32, HEIGHT as i32).unwrap();

    Ok((pipeline, event_queue))
}

fn main_loop((pipeline, event_queue): (gst::Pipeline, &mut EventQueue)) -> Result<(), Error> {
    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline
        .get_bus()
        .expect("Pipeline without bus. Shouldn't happen!");

    gst::glib::MainContext::default().acquire();

    bus.add_watch_local(move |bus, msg| {
        use gst::MessageView;

        match msg.view() {
            MessageView::Eos(eos) => {
                println!("Eos: {:#?}\n{:#?}", bus, eos);
            }
            MessageView::Error(err) => {
                eprintln!("Error: {:#?}\n{:#?}", bus, err);
                pipeline.set_state(gst::State::Null).unwrap();
            }
            _ => {
                println!("Unhandled: {:#?}\n{:#?}", bus, msg);
            }
        }
        gst::glib::Continue(true)
    })
        .expect("Failed to add bus watch");

    loop {
        event_queue.dispatch(&mut (), |_, _, _| { /* we ignore unfiltered messages */ }).unwrap();
    }
}

fn main() {
    let display = Display::connect_to_env().unwrap();
    let mut event_queue = display.create_event_queue();
    let attached_display = (*display).clone().attach(event_queue.token());
    let globals = GlobalManager::new(&attached_display);

    // Make a synchronized roundtrip to the wayland server.
    //
    // When this returns it must be true that the server has already
    // sent us all available globals.
    event_queue.sync_roundtrip(&mut (), |_, _, _| unreachable!()).unwrap();

    /*
     * Create a buffer with window contents
     */

    // buffer (and window) width and height
    let buf_x: u32 = 640;
    let buf_y: u32 = 480;

    // create a tempfile to write the contents of the window on
    let mut tmp = tempfile::tempfile().expect("Unable to create a tempfile.");
    // write the contents to it, lets put a nice color gradient
    for i in 0..(buf_x * buf_y) {
        let x = i % buf_x;
        let y = i / buf_x;
        let a = 0xFF;
        let r = min(((buf_x - x) * 0xFF) / buf_x, ((buf_y - y) * 0xFF) / buf_y);
        let g = min((x * 0xFF) / buf_x, ((buf_y - y) * 0xFF) / buf_y);
        let b = min(((buf_x - x) * 0xFF) / buf_x, (y * 0xFF) / buf_y);
        tmp.write_all(&((a << 24) + (r << 16) + (g << 8) + b).to_ne_bytes()).unwrap();
    }
    let _ = tmp.flush();

    /*
     * Init wayland objects
     */

    // The compositor allows us to creates surfaces
    let compositor = globals.instantiate_exact::<wl_compositor::WlCompositor>(1).unwrap();
    let surface = compositor.create_surface();

    // The SHM allows us to share memory with the server, and create buffers
    // on this shared memory to paint our surfaces
    let shm = globals.instantiate_exact::<wl_shm::WlShm>(1).unwrap();
    let pool = shm.create_pool(
        tmp.as_raw_fd(),            // RawFd to the tempfile serving as shared memory
        (buf_x * buf_y * 4) as i32, // size in bytes of the shared memory (4 bytes per pixel)
    );
    let buffer = pool.create_buffer(
        0,                        // Start of the buffer in the pool
        buf_x as i32,             // width of the buffer in pixels
        buf_y as i32,             // height of the buffer in pixels
        (buf_x * 4) as i32,       // number of bytes between the beginning of two consecutive lines
        wl_shm::Format::Argb8888, // chosen encoding for the data
    );

    let xdg_wm_base = globals
        .instantiate_exact::<xdg_wm_base::XdgWmBase>(2)
        .expect("Compositor does not support xdg_shell");

    xdg_wm_base.quick_assign(|xdg_wm_base, event, _| {
        if let xdg_wm_base::Event::Ping { serial } = event {
            xdg_wm_base.pong(serial);
        };
    });

    let xdg_surface = xdg_wm_base.get_xdg_surface(&surface);
    xdg_surface.quick_assign(move |xdg_surface, event, _| match event {
        xdg_surface::Event::Configure { serial } => {
            println!("xdg_surface (Configure)");
            xdg_surface.ack_configure(serial);
        }
        _ => unreachable!(),
    });

    let xdg_toplevel = xdg_surface.get_toplevel();
    xdg_toplevel.quick_assign(move |_, event, _| {
        match event {
            xdg_toplevel::Event::Close => {
                exit(0);
            }
            xdg_toplevel::Event::Configure { width, height, states } => {
                println!("xdg_toplevel (Configure) width: {}, height: {}, states: {:?}",
                         width, height, states);
            }
            _ => unreachable!(),
        }
    });
    xdg_toplevel.set_title("Simple Window".to_string());

    // initialize a seat to retrieve pointer & keyboard events
    //
    // example of using a common filter to handle both pointer & keyboard events
    let common_filter = Filter::new(move |event, _, _| match event {
        Events::Pointer { event, .. } => match event {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } => {
                println!("Pointer entered at ({}, {}).", surface_x, surface_y);
            }
            wl_pointer::Event::Leave { .. } => {
                println!("Pointer left.");
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                println!("Pointer moved to ({}, {}).", surface_x, surface_y);
            }
            wl_pointer::Event::Button { button, state, .. } => {
                println!("Button {} was {:?}.", button, state);
            }
            _ => {}
        },
        Events::Keyboard { event, .. } => match event {
            wl_keyboard::Event::Enter { .. } => {
                println!("Gained keyboard focus.");
            }
            wl_keyboard::Event::Leave { .. } => {
                println!("Lost keyboard focus.");
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                println!("Key with id {} was {:?}.", key, state);
            }
            _ => (),
        },
    });
    // to be handled properly this should be more dynamic, as more
    // than one seat can exist (and they can be created and destroyed
    // dynamically), however most "traditional" setups have a single
    // seat, so we'll keep it simple here
    let mut pointer_created = false;
    let mut keyboard_created = false;
    globals.instantiate_exact::<wl_seat::WlSeat>(1).unwrap().quick_assign(move |seat, event, _| {
        // The capabilities of a seat are known at runtime and we retrieve
        // them via an events. 3 capabilities exists: pointer, keyboard, and touch
        // we are only interested in pointer & keyboard here
        use wayland_client::protocol::wl_seat::{Capability, Event as SeatEvent};

        if let SeatEvent::Capabilities { capabilities } = event {
            if !pointer_created && capabilities.contains(Capability::Pointer) {
                // create the pointer only once
                pointer_created = true;
                seat.get_pointer().assign(common_filter.clone());
            }
            if !keyboard_created && capabilities.contains(Capability::Keyboard) {
                // create the keyboard only once
                keyboard_created = true;
                seat.get_keyboard().assign(common_filter.clone());
            }
        }
    });

    event_queue.sync_roundtrip(&mut (), |_, _, _| { /* we ignore unfiltered messages */ }).unwrap();

    surface.attach(Some(&buffer), 0, 0);
    surface.commit();

    match create_pipeline(surface, display, &mut event_queue).and_then(main_loop) {
        Ok(r) => r,
        Err(e) => eprintln!("Error! {}", e),
    }
}
