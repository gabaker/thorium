//! `thoradm schema print`: prints the declared schema

use thorium::Conf;
use thorium::models::schema::{SchemaObject, SnapshotObject, render};
use thorium::models::upgrades::SchemaBackend;

use crate::Error;
use crate::args::{Args, PrintSchema};
use crate::outcome::RunOutcome;

/// Print the declared schema, as CQL or in canonical form
///
/// # Arguments
///
/// * `cmd` - Which backend to print and in what form
/// * `args` - The arguments thoradm was started with
#[expect(
    clippy::unused_async,
    reason = "every schema handler is async so the dispatcher awaits them alike"
)]
pub(super) async fn handle(cmd: &PrintSchema, args: &Args) -> Result<RunOutcome, Error> {
    // render the schema, loading the config only when the CQL needs it
    print!("{}", print_text(cmd, || super::load_conf(args))?);
    Ok(RunOutcome::Complete)
}

/// Render the declared schema: the CQL `schema apply` issues on an empty cluster, or the canonical
/// JSON form, which is the only form the Elastic indexes and the Redis and S3 layouts have
///
/// # Arguments
///
/// * `cmd` - Which backend to print and in what form
/// * `conf` - Loads the config, for the keyspace and table TTLs the CQL names
fn print_text(
    cmd: &PrintSchema,
    conf: impl FnOnce() -> Result<Conf, Error>,
) -> Result<String, Error> {
    // the canonical form, and every backend without CQL, prints canonical JSON
    let cql = !cmd.canonical && matches!(cmd.backend, None | Some(SchemaBackend::Scylla));
    if !cql {
        let objects = SnapshotObject::declared()
            .iter()
            .filter(|object| cmd.backend.is_none_or(|backend| object.backend == backend))
            .collect::<Vec<_>>();
        let mut text = serde_json::to_string_pretty(&objects)?;
        text.push('\n');
        return Ok(text);
    }
    // the CQL is every statement `schema apply` issues on an empty cluster
    let conf = conf()?;
    let mut text = render(&conf, &SchemaObject::declared()).join(";\n");
    text.push_str(";\n");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use thorium::Conf;
    use thorium::models::schema::{SchemaObject, SnapshotObject, render};
    use thorium::models::upgrades::SchemaBackend;

    use super::print_text;
    use crate::Error;
    use crate::args::PrintSchema;

    /// Load the test config
    fn conf() -> Conf {
        Conf::new("../api/tests/thorium-testing.yml").expect("the test config loads")
    }

    /// The default form is every statement `schema apply` issues on an empty cluster, each ending
    /// with a semicolon
    #[test]
    fn print_renders_the_apply_statements() {
        // render the declared schema as CQL
        let cmd = PrintSchema {
            backend: None,
            canonical: false,
        };
        let text = print_text(&cmd, || Ok::<_, Error>(conf())).expect("the schema renders");
        let mut expected = render(&conf(), &SchemaObject::declared()).join(";\n");
        expected.push_str(";\n");
        assert!(text.starts_with("CREATE KEYSPACE IF NOT EXISTS "), "{text}");
        assert_eq!(text, expected);
    }

    /// The canonical form, and any backend other than Scylla, prints the declared objects'
    /// canonical JSON, filtered to the backend, without reading the config
    #[test]
    fn print_renders_canonical_json_per_backend() {
        // a backend without CQL prints its canonical objects, never loading the config
        let cmd = PrintSchema {
            backend: Some(SchemaBackend::Redis),
            canonical: false,
        };
        let text = print_text(&cmd, || panic!("the config isn't needed")).expect("renders");
        let parsed: Vec<SnapshotObject> = serde_json::from_str(&text).expect("canonical JSON");
        let expected = SnapshotObject::declared()
            .iter()
            .filter(|object| object.backend == SchemaBackend::Redis)
            .cloned()
            .collect::<Vec<_>>();
        assert!(expected.len() > 1, "{expected:?}");
        assert_eq!(parsed, expected);
        // the canonical flag without a backend prints every declared object
        let cmd = PrintSchema {
            backend: None,
            canonical: true,
        };
        let text = print_text(&cmd, || panic!("the config isn't needed")).expect("renders");
        let parsed: Vec<SnapshotObject> = serde_json::from_str(&text).expect("canonical JSON");
        assert_eq!(parsed, SnapshotObject::declared());
    }
}
