//! Tests of the data-shaping record's pure parts: the files password's identity, the record's
//! rows, the names settings are reported by, the tables sampled and the sizes their rows
//! suggest, and how refusals are reported

use argon2::password_hash::{PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use chrono::{DateTime, Duration, Utc};

use super::config_record::{
    DATA_TYPES, FILES_PASSWORD, Sample, bucketed, check_files_password_hash,
    default_password_warning, hash_files_password, seconds_into, setting_id, setting_path,
    verify_files_password,
};
use super::{ApplyError, ConfigRecord};
use crate::Conf;
use crate::conf::DEFAULT_FILES_PASSWORD;
use crate::utils::helpers::partition;

/// The test config, whose files password is the public default
fn conf() -> Conf {
    // the integration tests' config, with its placeholders left in place
    let text = include_str!("../../../tests/thorium-testing.yml");
    let file = config::File::from_str(text, config::FileFormat::Yaml);
    config::Config::builder()
        .add_source(file)
        .build()
        .and_then(config::Config::try_deserialize)
        .expect("the test config parses")
}

/// Swap one parameter field of a PHC string, such as `m=19456`, for another
///
/// # Arguments
///
/// * `phc` - The PHC string
/// * `from` - The text to replace
/// * `to` - The text to put in its place
fn swap(phc: &str, from: &str, to: &str) -> String {
    // the field must be there for the swap to mean anything
    assert!(phc.contains(from), "{phc} lacks {from}");
    phc.replacen(from, to, 1)
}

/// The identity is an argon2id PHC string with the default parameters, which verifies against
/// the password it was made from and no other
#[test]
fn the_password_identity_is_argon2id_with_default_parameters() {
    // hash a password
    let phc = hash_files_password("corn-on-the-cob").expect("the password hashes");
    assert!(
        phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{phc} isn't argon2id with the default parameters"
    );
    assert!(!phc.contains("corn-on-the-cob"), "{phc}");
    // it verifies against the same password only
    assert!(verify_files_password(&phc, "corn-on-the-cob").expect("the identity is accepted"));
    assert!(!verify_files_password(&phc, "corn-on-the-cob ").expect("the identity is accepted"));
    // a second hash of the same password has another salt, and verifies too
    let again = hash_files_password("corn-on-the-cob").expect("the password hashes");
    assert_ne!(phc, again);
    assert!(verify_files_password(&again, "corn-on-the-cob").expect("the identity is accepted"));
}

/// An identity with another algorithm, version, cost, or output length, or no salt or hash, is
/// refused by the parameter check, which computes no hash
#[test]
fn identities_with_other_parameters_are_refused() {
    // start from a real identity of the password
    let phc = hash_files_password("corn").expect("the password hashes");
    assert!(check_files_password_hash(&phc).is_ok());
    // each change of parameters is refused, even though the hash part is unchanged
    let refused = [
        swap(&phc, "$argon2id$", "$argon2i$"),
        swap(&phc, "$argon2id$", "$argon2d$"),
        swap(&phc, "v=19", "v=16"),
        swap(&phc, "m=19456", "m=19457"),
        swap(&phc, "m=19456", "m=8"),
        swap(&phc, "t=2", "t=1"),
        swap(&phc, "t=2", "t=3"),
        swap(&phc, "p=1", "p=2"),
        // a key id or associated data, which the default parameters don't have
        swap(&phc, "p=1", "p=1,keyid=a2V5aWQ"),
        swap(&phc, "p=1", "p=1,data=ZGF0YQ"),
        // passes so many that computing them would keep a core busy for hours, in little memory
        swap(&phc, "m=19456,t=2", "m=8,t=4294967295"),
        "$argon2id$v=19$m=19456,t=2,p=1".to_owned(),
        "not a phc string".to_owned(),
        String::new(),
    ];
    for phc in refused {
        assert!(
            matches!(
                check_files_password_hash(&phc),
                Err(ApplyError::PasswordHashParams)
            ),
            "{phc} was accepted"
        );
        assert!(
            matches!(
                verify_files_password(&phc, "corn"),
                Err(ApplyError::PasswordHashParams)
            ),
            "{phc} was verified"
        );
    }
    // a real hash with a shorter output, which only the output length tells apart, is refused
    let params = Params::new(
        Params::DEFAULT_M_COST,
        Params::DEFAULT_T_COST,
        Params::DEFAULT_P_COST,
        Some(16),
    )
    .expect("the parameters are valid");
    let salt = SaltString::encode_b64(&[7; 16]).expect("the salt encodes");
    let short = Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password(b"corn", &salt)
        .expect("the password hashes")
        .to_string();
    assert!(
        short.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{short}"
    );
    assert!(matches!(
        check_files_password_hash(&short),
        Err(ApplyError::PasswordHashParams)
    ));
    assert!(matches!(
        verify_files_password(&short, "corn"),
        Err(ApplyError::PasswordHashParams)
    ));
}

/// The record from a config holds every data type's partition size and earliest value, tags per
/// tag type, and the password's identity, and its rows read back to the same record
#[test]
fn the_record_rows_round_trip() {
    // build the record of the test config
    let conf = conf();
    let record = ConfigRecord::from_conf(&conf).expect("the record builds");
    let keys = DATA_TYPES
        .iter()
        .map(|key| (*key).to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        record.partition_sizes.keys().cloned().collect::<Vec<_>>(),
        {
            let mut sorted = keys.clone();
            sorted.sort();
            sorted
        }
    );
    assert_eq!(
        record.partition_sizes["files"],
        conf.thorium.files.partition_size
    );
    assert_eq!(
        record.partition_sizes["tags.entities"],
        conf.thorium.tags.entities.partition_size
    );
    assert_eq!(record.earliest["repos"], conf.thorium.repos.earliest);
    assert_eq!(
        record.earliest["tags.files"],
        conf.thorium.tags.files.earliest
    );
    assert!(verify_files_password(&record.files_password, DEFAULT_FILES_PASSWORD).unwrap());
    // the rows name each setting once, and read back to the same record
    let rows = record.rows();
    assert_eq!(rows.len(), 2 * DATA_TYPES.len() + 1);
    let ids = rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>();
    assert!(ids.contains(&"partition_size.files"), "{ids:?}");
    assert!(ids.contains(&"partition_size.tags.repos"), "{ids:?}");
    assert!(ids.contains(&"earliest.associations"), "{ids:?}");
    assert!(ids.contains(&FILES_PASSWORD), "{ids:?}");
    assert_eq!(
        ConfigRecord::from_rows(rows).expect("the rows read"),
        record
    );
}

/// A record read from only some rows keeps what's there, and an unreadable number is an error
#[test]
fn a_partial_record_reads_back() {
    // only a size and the password
    let rows = vec![
        ("partition_size.files".to_owned(), "3600".to_owned()),
        ("unknown.setting".to_owned(), "x".to_owned()),
    ];
    let record = ConfigRecord::from_rows(rows).expect("the rows read");
    assert_eq!(record.partition_sizes.len(), 1);
    assert_eq!(record.partition_sizes["files"], 3600);
    assert!(record.earliest.is_empty());
    assert_eq!(record.files_password.len(), 0);
    assert!(record.rows().iter().all(|(id, _)| id != FILES_PASSWORD));
    // a size that isn't a number can't be compared
    let bad = vec![("partition_size.files".to_owned(), "big".to_owned())];
    assert!(ConfigRecord::from_rows(bad).is_err());
}

/// Settings are reported by their config paths, and the paths map back to the record ids
#[test]
fn settings_are_named_by_their_config_paths() {
    // each kind of setting
    let names = [
        ("partition_size.files", "thorium.files.partition_size"),
        (
            "partition_size.tags.repos",
            "thorium.tags.repos.partition_size",
        ),
        ("earliest.results", "thorium.results.earliest"),
        ("earliest.tags.entities", "thorium.tags.entities.earliest"),
        ("files.password", "thorium.files.password"),
    ];
    for (id, path) in names {
        assert_eq!(setting_path(id), path);
        assert_eq!(setting_id(path), id);
    }
}

/// Only the public default files password raises the warning, which names the setting and never
/// a password
#[test]
fn the_default_password_is_warned_about() {
    // the test config uses the default
    let mut conf = conf();
    let warning = default_password_warning(&conf).expect("the default is warned about");
    assert!(warning.contains("thorium.files.password"), "{warning}");
    assert!(!warning.contains(DEFAULT_FILES_PASSWORD), "{warning}");
    // any other password raises nothing
    conf.thorium.files.password = "something-else".to_owned();
    assert_eq!(default_password_warning(&conf), None);
}

/// Every declared table with a time bucket is found, with the timestamp its bucket comes from and
/// the data type whose size buckets it
#[test]
fn every_bucketed_table_is_sampled_with_its_timestamp() {
    // the declared Scylla objects
    let objects = super::SchemaObject::declared()
        .into_iter()
        .filter_map(|object| match object.definition {
            super::SchemaDefinition::Scylla(def) => Some(def),
            _ => None,
        })
        .collect::<Vec<_>>();
    let partitioned = objects
        .iter()
        .filter_map(|object| match object {
            super::ObjectDef::Table(table) if table.partition_size.is_some() => Some(table.name),
            _ => None,
        })
        .collect::<Vec<_>>();
    let found = bucketed(&objects);
    // every partitioned table is found once, none is skipped
    assert_eq!(
        found.iter().map(|table| table.table).collect::<Vec<_>>(),
        partitioned
    );
    let by_name = found
        .iter()
        .map(|table| (table.table, table))
        .collect::<std::collections::BTreeMap<_, _>>();
    // the main tables of each data type, with the timestamps their buckets come from
    let samples = &by_name["samples_list"];
    assert_eq!(samples.data_type.as_deref(), Some("files"));
    assert_eq!(samples.timestamp, "uploaded");
    assert_eq!(samples.partition, vec!["group", "year", "bucket"]);
    assert_eq!(by_name["repos_list"].data_type.as_deref(), Some("repos"));
    assert_eq!(by_name["commitish_list"].timestamp, "timestamp");
    assert_eq!(by_name["entities"].timestamp, "created");
    assert_eq!(
        by_name["associations"].data_type.as_deref(),
        Some("associations")
    );
    assert_eq!(
        by_name["results_stream"].data_type.as_deref(),
        Some("results")
    );
    // the tags table picks its size by each row's type
    assert_eq!(by_name["tags"].data_type, None);
    // every data type the record keys by has a table to sample, the tag types through the tags
    for data_type in DATA_TYPES {
        let covered = found.iter().any(|table| match &table.data_type {
            Some(owner) => owner == data_type,
            None => data_type.starts_with("tags."),
        });
        assert!(covered, "{data_type} has no table to sample");
    }
}

/// The mismatched rows suggest the one size that explains them all: the common size among those
/// that do, the only size that does, or none
#[test]
fn mismatched_rows_suggest_the_size_that_explains_them() {
    // rows bucketed at 3600 seconds, checked against 180
    let mut sample = Sample::default();
    for seconds in [86_400 * 40 + 17, 86_400 * 41 + 3_599, 86_400 * 90] {
        let bucket = i32::try_from(seconds / 3600).unwrap();
        sample.mismatch(seconds, bucket);
    }
    assert_eq!(sample.mismatched, 3);
    assert_eq!(sample.suggested(), Some(3600));
    // rows at 600 seconds suggest 600
    let mut sample = Sample::default();
    for seconds in [1_000_000, 2_345_678, 9_999_999] {
        sample.mismatch(seconds, i32::try_from(seconds / 600).unwrap());
    }
    assert_eq!(sample.suggested(), Some(600));
    // rows at an uncommon size narrow to that size alone
    let mut sample = Sample::default();
    for seconds in [700_000, 700_000 + 1_234, 1_000_000 + 1_233, 5_000_000] {
        sample.mismatch(seconds, i32::try_from(seconds / 1_234).unwrap());
    }
    assert_eq!(sample.suggested(), Some(1_234));
    // rows two sizes can't both explain suggest nothing
    let mut sample = Sample::default();
    sample.mismatch(1_000_000, 1_000_000 / 3600);
    sample.mismatch(1_000_000, 1_000_000 / 600);
    assert_eq!(sample.suggested(), None);
    // rows in the first bucket fit every larger size, so no single common size explains them
    let mut sample = Sample::default();
    sample.mismatch(30, 0);
    assert_eq!(sample.suggested(), None);
    // a negative bucket fits no size
    let mut sample = Sample::default();
    sample.mismatch(5_000, -1);
    assert_eq!(sample.suggested(), None);
    // with no mismatch there's nothing to suggest
    assert_eq!(Sample::default().suggested(), None);
}

/// A refusal for sampled rows in other buckets names the data type and how to record the
/// configured size anyway, with or without a suggested size
#[test]
fn sampled_refusals_name_how_to_accept() {
    // a refusal with a suggested size names the data type, the suggestion, and the accept flag
    let suggested = ApplyError::SampledSizeMismatch {
        data_type: "entities".to_owned(),
        mismatched: 64,
        suggested: Some(3600),
    };
    let text = suggested.to_string();
    assert!(text.contains("64 sampled entities rows"), "{text}");
    assert!(text.contains("partition size of 3600"), "{text}");
    assert!(
        text.contains("thoradm schema apply --accept-partition-sizes"),
        "{text}"
    );
    // a refusal with no suggested size still names the accept flag
    let unexplained = ApplyError::SampledSizeMismatch {
        data_type: "entities".to_owned(),
        mismatched: 3,
        suggested: None,
    };
    let text = unexplained.to_string();
    assert!(!text.contains("they suggest"), "{text}");
    assert!(
        text.contains("thoradm schema apply --accept-partition-sizes"),
        "{text}"
    );
    // the operator's approval passes the same flag, which the message says
    assert!(text.contains("operator"), "{text}");
}

/// A refusal names the setting and how a deliberate change goes through, the password's never
/// prints a value, and the operator reads the refused settings and sampled types from any shape
#[test]
fn refusals_name_their_settings() {
    // a changed partition size names both values and the way out
    let size = ApplyError::ConfigChanged {
        setting: "thorium.files.partition_size".to_owned(),
        recorded: Some("180".to_owned()),
        configured: Some("3600".to_owned()),
    };
    let text = size.to_string();
    assert!(text.contains("thorium.files.partition_size"), "{text}");
    assert!(text.contains("180") && text.contains("3600"), "{text}");
    assert!(text.contains("rebucket-files"), "{text}");
    // the password's refusal names only the setting
    let password = ApplyError::ConfigChanged {
        setting: "thorium.files.password".to_owned(),
        recorded: None,
        configured: None,
    };
    assert!(
        password.to_string().contains("thorium.files.password"),
        "{password}"
    );
    let sampled = ApplyError::SampledSizeMismatch {
        data_type: "tags.files".to_owned(),
        mismatched: 4,
        suggested: Some(3600),
    };
    // one refusal or several give the same lists
    assert_eq!(
        size.refused_settings(),
        vec!["thorium.files.partition_size"]
    );
    assert_eq!(size.sampled_data_types().len(), 0);
    assert_eq!(sampled.sampled_data_types(), vec!["tags.files"]);
    assert_eq!(sampled.refused_settings().len(), 0);
    assert_eq!(password.refused_settings(), vec!["thorium.files.password"]);
    // a recorded identity with other parameters refuses the password too
    assert_eq!(
        ApplyError::PasswordHashParams.refused_settings(),
        vec!["thorium.files.password"]
    );
    let all = ApplyError::Refused(vec![size, sampled, ApplyError::PasswordHashParams]);
    assert_eq!(
        all.refused_settings(),
        vec!["thorium.files.partition_size", "thorium.files.password"]
    );
    assert_eq!(all.sampled_data_types(), vec!["tags.files"]);
    let text = all.to_string();
    assert!(
        text.contains("rebucket-files") && text.contains("4 sampled"),
        "{text}"
    );
}

/// Buckets count from one second past the start of the year, and the bounds a mismatched row sets
/// hold exactly at a bucket's edges
#[test]
fn bucket_bounds_hold_at_their_edges() {
    // the first second buckets count from, and the start of the next hour's bucket
    let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:01Z")
        .expect("the time parses")
        .with_timezone(&Utc);
    assert_eq!(seconds_into(start, 2026), Some(0));
    assert_eq!(
        seconds_into(start + Duration::seconds(3600), 2026),
        Some(3600)
    );
    assert_eq!(partition(start + Duration::seconds(3599), 2026, 3600), 0);
    assert_eq!(partition(start + Duration::seconds(3600), 2026, 3600), 1);
    // a year no date can hold has no seconds
    assert_eq!(seconds_into(start, 300_000), None);
    // a row in the first bucket at second 1233 needs a size above 1233, and one in the second
    // bucket at second 1234 a size of at most 1234, which leaves only 1234
    let mut sample = Sample::default();
    sample.mismatch(1_233, 0);
    sample.mismatch(1_234, 1);
    assert_eq!(sample.suggested(), Some(1_234));
    // one more second in the first bucket leaves no size
    let mut sample = Sample::default();
    sample.mismatch(1_234, 0);
    sample.mismatch(1_234, 1);
    assert_eq!(sample.suggested(), None);
}
