

fn main() {
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);
    let filter_layer = tracing_subscriber::EnvFilter::from_env("DENPA_LOG");

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    tracing_gstreamer::integrate_events();
    gstreamer::debug_set_default_threshold(gstreamer::DebugLevel::Debug);
    gstreamer::debug_remove_default_log_function();
    gstreamer::init();


}
