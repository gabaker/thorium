//! The Thorium event handler

use clap::Parser;

use thorium_event_handler::args;
use thorium_event_handler::libs::EventWorkerController;

#[tokio::main]
async fn main() {
    // get command line args
    let args = args::Args::parse();
    // try to load a config file
    let conf = thorium::Conf::new(&args.config).expect("Failed to load config");
    // setup our tracer
    let trace_provider = thorium::utils::trace::setup("ThoriumEventHandler", &conf.thorium.tracing);
    // build our event controller, retrying until the API can serve its caches
    let controller = Box::pin(EventWorkerController::new_with_retry(args, conf)).await;
    // start our event handler workers
    controller.start().await.expect("Event controller failed!");
    // export any remaining traces and shutdown this provider
    thorium::utils::trace::shutdown(trace_provider);
}
