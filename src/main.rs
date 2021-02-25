extern crate gstreamer as gst;
extern crate gstreamer_app as gst_app;
extern crate gstreamer_video as gst_video;
extern crate smithay_client_toolkit as sctk;

use std::cmp::min;
use std::ffi::c_void;
use std::io::{BufWriter, Seek, SeekFrom, Write};

use anyhow::Error;
use derive_more::{Display, Error};
use gst::prelude::*;
use gst_video::prelude::*;
use sctk::reexports::client::Display;
use sctk::reexports::client::protocol::{wl_shm, wl_surface};
use sctk::shm::MemPool;
use sctk::window::{ButtonColorSpec, ColorSpec, ConceptConfig, ConceptFrame, Event as WEvent};

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

const WIDTH: usize = 640;
const HEIGHT: usize = 480;
const GST_WAYLAND_DISPLAY_HANDLE_CONTEXT_TYPE: &str = "GstWaylandDisplayHandleContextType";

sctk::default_environment!(ThemedFrameExample, desktop);

fn create_pipeline(surface: &wl_surface::WlSurface, display: Display) -> Result<gst::Pipeline, Error> {
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

    Ok(pipeline)
}

fn main() {
    let (env, display, mut queue) = sctk::new_default_environment!(ThemedFrameExample, desktop)
        .expect("Unable to connect to a Wayland compositor");

    let mut dimensions = (640u32, 480u32);

    let surface = env.create_surface().detach();

    let mut window = env
        .create_window::<ConceptFrame, _>(
            surface,
            None,
            dimensions,
            move |evt, mut dispatch_data| {
                let next_action = dispatch_data.get::<Option<WEvent>>().unwrap();
                // Keep last event in priority order : Close > Configure > Refresh
                let replace = match (&evt, &*next_action) {
                    (_, &None)
                    | (_, &Some(WEvent::Refresh))
                    | (&WEvent::Configure { .. }, &Some(WEvent::Configure { .. }))
                    | (&WEvent::Close, _) => true,
                    _ => false,
                };
                if replace {
                    *next_action = Some(evt);
                }
            },
        )
        .expect("Failed to create a window !");

    window.set_title("Themed frame".to_string());
    window.set_frame_config(create_frame_config());

    let mut pools = env.create_double_pool(|_| {}).expect("Failed to create a memory pool !");

    if !env.get_shell().unwrap().needs_configure() {
        // initial draw to bootstrap on wl_shell
        if let Some(pool) = pools.pool() {
            redraw(pool, window.surface(), dimensions).expect("Failed to draw")
        }
        window.refresh();
    }

    let pipeline = create_pipeline(window.surface(), display).unwrap();

    pipeline.set_state(gst::State::Playing).unwrap();

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

    let mut next_action = None;

    loop {
        match next_action.take() {
            Some(WEvent::Close) => break,
            Some(WEvent::Refresh) => {
                window.refresh();
                window.surface().commit();
            }
            Some(WEvent::Configure { new_size, states }) => {
                if let Some((w, h)) = new_size {
                    window.resize(w, h);
                    dimensions = (w, h)
                }
                println!("Window states: {:?}", states);
                window.refresh();
                if let Some(pool) = pools.pool() {
                    redraw(pool, window.surface(), dimensions).expect("Failed to draw")
                }
            }
            None => {}
        }

        queue.dispatch(&mut next_action, |_, _, _| {}).unwrap();
    }
}

// The frame configuration we will use in this example
fn create_frame_config() -> ConceptConfig {
    let icon_spec = ButtonColorSpec {
        hovered: ColorSpec::identical([0xFF, 0x22, 0x22, 0x22].into()),
        idle: ColorSpec::identical([0xFF, 0xff, 0xff, 0xff].into()),
        disabled: ColorSpec::invisible(),
    };

    ConceptConfig {
        // dark theme
        primary_color: ColorSpec {
            active: [0xFF, 0x22, 0x22, 0x22].into(),
            inactive: [0xFF, 0x33, 0x33, 0x33].into(),
        },
        // white separation line
        secondary_color: ColorSpec::identical([0xFF, 0xFF, 0xFF, 0xFF].into()),
        // red close button
        close_button: Some((
            // icon
            icon_spec,
            // button background
            ButtonColorSpec {
                hovered: ColorSpec::identical([0xFF, 0xFF, 0x00, 0x00].into()),
                idle: ColorSpec::identical([0xFF, 0x88, 0x00, 0x00].into()),
                disabled: ColorSpec::invisible(),
            },
        )),
        // green maximize button
        maximize_button: Some((
            // icon
            icon_spec,
            // button background
            ButtonColorSpec {
                hovered: ColorSpec::identical([0xFF, 0x00, 0xFF, 0x00].into()),
                idle: ColorSpec::identical([0xFF, 0x00, 0x88, 0x00].into()),
                disabled: ColorSpec::invisible(),
            },
        )),
        // blue minimize button
        minimize_button: Some((
            // icon
            icon_spec,
            // button background
            ButtonColorSpec {
                hovered: ColorSpec::identical([0xFF, 0x00, 0x00, 0xFF].into()),
                idle: ColorSpec::identical([0xFF, 0x00, 0x00, 0x88].into()),
                disabled: ColorSpec::invisible(),
            },
        )),
        // same font as default
        title_font: Some(("sans".into(), 17.0)),
        // clear text over dark background
        title_color: ColorSpec::identical([0xFF, 0xD0, 0xD0, 0xD0].into()),
    }
}


fn redraw(
    pool: &mut MemPool,
    surface: &wl_surface::WlSurface,
    (buf_x, buf_y): (u32, u32),
) -> Result<(), ::std::io::Error> {
    // resize the pool if relevant
    pool.resize((4 * buf_x * buf_y) as usize).expect("Failed to resize the memory pool.");
    // write the contents, a nice color gradient =)
    pool.seek(SeekFrom::Start(0))?;
    {
        let mut writer = BufWriter::new(&mut *pool);
        for i in 0..(buf_x * buf_y) {
            let x = (i % buf_x) as u32;
            let y = (i / buf_x) as u32;
            let r: u32 = min(((buf_x - x) * 0xFF) / buf_x, ((buf_y - y) * 0xFF) / buf_y);
            let g: u32 = min((x * 0xFF) / buf_x, ((buf_y - y) * 0xFF) / buf_y);
            let b: u32 = min(((buf_x - x) * 0xFF) / buf_x, (y * 0xFF) / buf_y);
            let pixel: u32 = (0xFF << 24) + (r << 16) + (g << 8) + b;
            writer.write_all(&pixel.to_ne_bytes())?;
        }
        writer.flush()?;
    }
    // get a buffer and attach it
    let new_buffer =
        pool.buffer(0, buf_x as i32, buf_y as i32, 4 * buf_x as i32, wl_shm::Format::Argb8888);
    surface.attach(Some(&new_buffer), 0, 0);
    surface.commit();
    Ok(())
}
