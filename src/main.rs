use axum::http::StatusCode;
use futures::StreamExt;
use glib::object::ObjectExt;
use glib::Cast;
use gst::glib;
use gst::prelude::{ElementExt, ElementExtManual, GstBinExt, GstObjectExt, PadExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::debug;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("could not build the tokio runtime")]
    BuildRuntime(#[source] std::io::Error),
    #[error("could not bind HTTP server to {1:}")]
    BindServer(#[source] std::io::Error, SocketAddr),
    #[error("could not serve the HTTP server")]
    ServeServer(#[source] std::io::Error),
    #[error("could not initialize gstreamer")]
    InitGst(#[source] glib::Error),
    #[error("could not register the gsthlssink3 plugin")]
    RegisterGstHlsSink(#[source] glib::BoolError),
    #[error("could not register the gstfmp4 plugin")]
    RegisterGstFmp4(#[source] glib::BoolError),
    #[error("gstreamer pipeline has no associated bus")]
    GstPipelineBus,
    #[error("could not start the gstreamer pipeline")]
    SetPlaying(#[source] gst::StateChangeError),
    #[error("could not set the gstreamer pipeline to the NULL state")]
    SetNullState(#[source] gst::StateChangeError),
    #[error("gstreamer pipeline error has occurred in {1}")]
    PipelineError(#[source] glib::Error, String),
    #[error("could not create a gstreamer element {1}")]
    CreateElement(#[source] glib::BoolError, &'static str),
    #[error("could not add the element {1} to a pipeline")]
    PipelineAdd(#[source] glib::BoolError, &'static str),
    #[error("could not link elements {1}")]
    LinkElement(#[source] gst::PadLinkError, &'static str),
}

#[derive(Default, Clone)]
struct Filesystem {
    store: Arc<Mutex<HashMap<String, File>>>,
}

impl Filesystem {
    fn create(&self, name: String) -> File {
        let mut guard = self.store.blocking_lock();
        let file = guard.entry(name).or_default().clone();
        file
    }

    fn remove(&self, name: &str) {
        let mut guard = self.store.blocking_lock();
        guard.remove(name);
    }

    async fn get(&self, name: &str) -> Option<File> {
        let guard = self.store.lock().await;
        guard.get(name).cloned()
    }
}

#[derive(Default, Clone)]
struct File(Arc<RwLock<Vec<u8>>>);

impl File {
    fn write(&self) -> impl std::io::Write {
        let mut guard = futures::executor::block_on(Arc::clone(&self.0).write_owned());
        guard.clear();
        FileStream(guard)
    }

    async fn data(&self) -> Vec<u8> {
        let guard = self.0.read().await;
        Vec::<u8>::clone(&guard)
    }
}

struct FileStream<T>(T);

impl<T: std::io::Write, D: std::ops::DerefMut<Target = T>> std::io::Write for FileStream<D> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn create_sink(name: &str, filesystem: Filesystem) -> Result<gst::Element, Error> {
    let sink = gst::ElementFactory::make("hlsfmp4sink")
        .name(name)
        .property("playlist-location", format!("{name}.m3u8"))
        .property("init-location", format!("{name}_%03d.mp4"))
        .property("location", format!("{name}_%04d.m4s"))
        .property("target-duration", 2u32)
        .property("enable-program-date-time", true)
        // Need sync=true for fmp4 to timeout properly in case of live pipeline
        .property("sync", true)
        .build()
        .map_err(|e| Error::CreateElement(e, "hlscmafsink"))?;

    let filesystem_clone = filesystem.clone();
    sink.connect_closure(
        "get-playlist-stream",
        false,
        glib::closure!(move |sink: &gst::Element, location: &str| {
            debug!("{}, writing playlist to {location}", sink.name());
            let file = filesystem_clone.create(location.to_string());
            gio::WriteOutputStream::new(file.write()).upcast::<gio::OutputStream>()
        }),
    );

    let filesystem_clone = filesystem.clone();
    sink.connect_closure(
        "get-init-stream",
        false,
        glib::closure!(move |sink: &gst::Element, location: &str| {
            debug!("{}, writing init segment to {location}", sink.name());
            let file = filesystem_clone.create(location.to_string());
            gio::WriteOutputStream::new(file.write()).upcast::<gio::OutputStream>()
        }),
    );

    let filesystem_clone = filesystem.clone();
    sink.connect_closure(
        "get-fragment-stream",
        false,
        glib::closure!(move |sink: &gst::Element, location: &str| {
            debug!("{}, writing segment to {location}", sink.name());
            let file = filesystem_clone.create(location.to_string());
            gio::WriteOutputStream::new(file.write()).upcast::<gio::OutputStream>()
        }),
    );

    let filesystem_clone = filesystem.clone();
    sink.connect_closure(
        "delete-fragment",
        false,
        glib::closure!(move |sink: &gst::Element, location: &str| {
            debug!("{}, removing segment {location}", sink.name());
            filesystem_clone.remove(location);
            true
        }),
    );

    Ok(sink)
}

fn setup_audio_sink(pipeline: &gst::Pipeline, filesystem: Filesystem) -> Result<(), Error> {
    let mut path = std::env::current_dir().unwrap();
    path.push("test16.flac");
    let src = gst::ElementFactory::make("uriplaylistbin")
        .property("uris", &[
            &*format!("file://{}", path.display()),
            &*format!("file://{}", path.display()),
        ][..])
        .build()
        .map_err(|e| Error::CreateElement(e, "uriplaylistbin"))?;
    let audioconvert = gst::ElementFactory::make("audioconvert")
        .build()
        .map_err(|e| Error::CreateElement(e, "audioconvert"))?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "S16LE")
        .build();
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .map_err(|e| Error::CreateElement(e, "capsfilter"))?;
    let rgvolume = gst::ElementFactory::make("rgvolume")
        .property("album-mode", true)
        .property("fallback-gain", -5.0)
        .build()
        .map_err(|e| Error::CreateElement(e, "rgvolume"))?;
    let enc = gst::ElementFactory::make("flacenc")
        .build()
        .map_err(|e| Error::CreateElement(e, "flacenc"))?;
    let parse = gst::ElementFactory::make("flacparse")
        .build()
        .map_err(|e| Error::CreateElement(e, "flacparse"))?;
    let sink = create_sink("flac", filesystem)?;

    let next_pad = audioconvert.static_pad("sink").unwrap();
    src.connect_pad_removed({
        let next_pad = next_pad.clone();
        move |_playlist, pad| {
            let _ = pad.unlink(&next_pad);
        }
    });
    src.connect_pad_added(move |_playlist, pad| {
        if pad.name().starts_with("audio") {
            if next_pad.is_linked() {
                tracing::error!("more than a single audio pad?");
                return;
            }
            pad.link(&next_pad).unwrap();
        }
    });

    pipeline
        .add(&src)
        .map_err(|e| Error::PipelineAdd(e, "uridecodebin3"))?;
    pipeline
        .add(&audioconvert)
        .map_err(|e| Error::PipelineAdd(e, "audioconvert"))?;
    pipeline
        .add(&capsfilter)
        .map_err(|e| Error::PipelineAdd(e, "capsfilter"))?;
    pipeline
        .add(&rgvolume)
        .map_err(|e| Error::PipelineAdd(e, "rgvolume"))?;
    pipeline
        .add(&enc)
        .map_err(|e| Error::PipelineAdd(e, "flacenc"))?;
    pipeline
        .add(&parse)
        .map_err(|e| Error::PipelineAdd(e, "flacparse"))?;
    pipeline
        .add(&sink)
        .map_err(|e| Error::PipelineAdd(e, "sink"))?;
    audioconvert
        .static_pad("src")
        .unwrap()
        .link(&capsfilter.static_pad("sink").unwrap())
        .map_err(|e| Error::LinkElement(e, "audioconvert | rgvolume"))?;
    capsfilter
        .static_pad("src")
        .unwrap()
        .link(&rgvolume.static_pad("sink").unwrap())
        .map_err(|e| Error::LinkElement(e, "rgvolume | audioconvert"))?;
    rgvolume
        .static_pad("src")
        .unwrap()
        .link(&enc.static_pad("sink").unwrap())
        .map_err(|e| Error::LinkElement(e, "audioconvert | flacenc"))?;
    enc.static_pad("src")
        .unwrap()
        .link(&parse.static_pad("sink").unwrap())
        .map_err(|e| Error::LinkElement(e, "flacenc | flacparse"))?;
    parse
        .static_pad("src")
        .unwrap()
        .link(&sink.request_pad_simple("sink_%u").unwrap())
        .map_err(|e| Error::LinkElement(e, "flacparse | sink"))?;
    Ok(())
}

fn run_radio(
    filesystem: Filesystem,
) -> Result<
    (
        glib::WeakRef<gst::Pipeline>,
        impl std::future::Future<Output = Result<(), Error>>,
    ),
    Error,
> {
    tracing_gstreamer::integrate_events();
    gst::debug_set_default_threshold(gst::DebugLevel::Trace);
    gst::debug_remove_default_log_function();
    gst::init().map_err(Error::InitGst)?;
    tracing_gstreamer::integrate_spans();
    gsthlssink3::plugin_register_static().map_err(Error::RegisterGstHlsSink)?;
    gstfmp4::plugin_register_static().map_err(Error::RegisterGstFmp4)?;
    let pipeline = gst::Pipeline::default();
    setup_audio_sink(&pipeline, filesystem)?;

    let bus = pipeline.bus().ok_or(Error::GstPipelineBus)?;
    let mut message_stream = bus.stream();

    pipeline
        .set_state(gst::State::Playing)
        .map_err(Error::SetPlaying)?;
    Ok((pipeline.downgrade(), async move {
        let result = loop {
            let message = message_stream.next().await;
            let Some(message) = message else {
                break Ok(())
            };
            match message.view() {
                gst::MessageView::Eos(..) => {
                    tracing::warn!("end of stream");
                    break Ok(());
                }
                gst::MessageView::Error(err) => {
                    let src = message
                        .src()
                        .map(|s| String::from(s.path_string()))
                        .unwrap_or_else(|| "None".into());
                    break Err(Error::PipelineError(err.error(), src));
                }
                gst::MessageView::Warning(w) => {
                    tracing::warn!(?w);
                }
                _ => {
                    tracing::info!(?message, "unhandled message");
                }
            }
        };
        tokio::task::spawn_blocking(move || {
            pipeline
                .set_state(gst::State::Null)
                .map_err(Error::SetNullState)
        })
        .await
        .unwrap()?;
        result
    }))
}

fn generate_master_playlist(_: &glib::WeakRef<gst::Pipeline>) -> Vec<u8> {
    let variants = vec![m3u8_rs::VariantStream {
        uri: "flac.m3u8".to_string(),
        bandwidth: 500_000,
        codecs: Some("audio/x-flac".into()),
        ..Default::default()
    }];

    let playlist = m3u8_rs::MasterPlaylist {
        version: Some(6),
        variants,
        independent_segments: true,
        ..Default::default()
    };

    let mut out = vec![];
    playlist
        .write_to(&mut out)
        .expect("Failed to write master playlist");
    out
}

async fn run_server(
    filesystem: Filesystem,
    gst_pipeline: glib::WeakRef<gst::Pipeline>,
) -> Result<(), Error> {
    let app = axum::Router::new()
        .route(
            "/*path",
            axum::routing::get({
                let filesystem = filesystem.clone();
                move |axum::extract::Path(path): axum::extract::Path<String>| {
                    let filesystem = filesystem.clone();
                    async move {
                        let Some(file) = filesystem.get(&path).await else {
                        return (StatusCode::NOT_FOUND, vec![])
                    };
                        (StatusCode::OK, file.data().await)
                    }
                }
            }),
        )
        .route(
            "/",
            axum::routing::get({
                let pipeline = gst_pipeline.clone();
                move || async move { generate_master_playlist(&pipeline) }
            }),
        );
    let addr: SocketAddr = ("0.0.0.0:3000").parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| Error::BindServer(e, addr))?;
    axum::serve(listener, app)
        .await
        .map_err(Error::ServeServer)?;
    Ok(())
}

fn run() -> Result<(), Error> {
    let log_sink = tracing_subscriber::fmt::layer();
    let log_filter = tracing_subscriber::EnvFilter::from_env("DENPA_LOG");
    tracing_subscriber::registry()
        .with(log_filter)
        .with(log_sink)
        .init();

    let filesystem = Filesystem::default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(4)
        .enable_all()
        .build()
        .map_err(Error::BuildRuntime)?;
    let (pipeline, radio) = run_radio(filesystem.clone())?;
    runtime.block_on(async {
        tokio::select! {
            result = run_server(filesystem.clone(), pipeline) => result,
            result = radio => result,
        }
    })
}

fn main() {
    std::process::exit(match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {}", e);
            let mut source = std::error::Error::source(&e);
            while let Some(e) = source {
                eprintln!("cause: {}", e);
                source = std::error::Error::source(e);
            }
            1
        }
    });
}
