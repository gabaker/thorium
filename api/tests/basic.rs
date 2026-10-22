//! Tests the basic routes in Thorium

use thorium::models::system::DEFAULT_IFF;
use thorium::{Error, is, test_utilities};

#[tokio::test]
async fn identify() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // send the identify query
    let resp = client.basic.identify().await?;
    // make sure we get the right string back
    is!(resp, DEFAULT_IFF);
    Ok(())
}

#[tokio::test]
async fn health() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // check Thorium's health
    let health = client.basic.health().await?;
    // make sure Thorium is healthy
    is!(health, true);
    Ok(())
}

/// The served `OpenAPI` documents name the version schema `ToolVersion`, never
/// `ImageVersion`
#[tokio::test]
async fn openapi_lists_tool_version() -> Result<(), Error> {
    // get an admin client so the test API is running
    test_utilities::admin_client().await?;
    // pick a loopback host when the API binds to a wildcard
    let host = match test_utilities::CONF.thorium.interface.as_str() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "[::1]",
        interface => interface,
    };
    // build a client that talks to the local API directly
    let client = reqwest::Client::builder().no_proxy().build()?;
    // check each OpenAPI document that lists the version type
    for docs in ["files", "images", "system"] {
        // get this OpenAPI document from the API
        let url = format!(
            "http://{host}:{}/api/{docs}/openapi.json",
            test_utilities::CONF.thorium.port
        );
        let raw = client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        // parse the document and get its schema components
        let doc: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|err| Error::new(format!("{url} is not valid JSON: {err}")))?;
        let schemas = &doc["components"]["schemas"];
        // make sure the version schema is listed as ToolVersion and not as ImageVersion
        is!(schemas.get("ToolVersion").is_some(), true);
        is!(schemas.get("ImageVersion").is_none(), true);
    }
    Ok(())
}

// Sync tests

#[cfg(all(feature = "sync", not(feature = "python")))]
#[test]
fn health_blocking() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client_blocking()?;
    // check Thorium's health
    let health = client.basic.health()?;
    // make sure Thorium is healthy
    is!(health, true);
    Ok(())
}
