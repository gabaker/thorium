// import any API only structures
cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        mod args;

        use clap::Parser;
    }
}

#[cfg(feature = "api")]
/// Start Thorium Api
#[tokio::main]
async fn main() {
    // load command line args
    let args = args::Args::parse();
    // load config
    let conf = thorium::conf::Conf::new(&args.config).expect("Failed to load config");
    // apply the schema first only when asked to
    let opts = thorium::utils::StartupOpts {
        apply_schema: args.apply_schema,
    };
    // launch our api, exiting with the reason when the applied schema can't be run on
    if let Err(err) = Box::pin(thorium::axum_with(conf, opts)).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

/// Main function alerting the user to compile the API with the api feature enabled
#[cfg(not(feature = "api"))]
fn main() {
    println!("To run the Thorium API please enable the API feature");
}
