//! Tests of the cursor runtime: checked limits and buckets, the bucket limit of a pull, and the
//! comparisons cursors follow from the declared clustering order

use chrono::{TimeZone, Utc};

use super::cursors::{cursor_order, order_by, resume_after, tie_from, tie_order, tie_predicates};
use super::{
    Bucket, BucketError, ClusterOrder, ColumnDef, ColumnRole, CqlDesc, CqlNative, CursorLimit,
    DEFAULT_LIMIT_CAP, ObjectDef, Order, STAGE_LOG_LIMIT_CAP, TableDef, bucket_limit,
};
use crate::utils::helpers;

/// A limit of 0 or above its route's cap is refused, and a limit plus one always fits an int
#[test]
fn cursor_limits_are_checked() {
    // 0 and anything past the cap are refused, whatever their size
    for raw in [
        0,
        DEFAULT_LIMIT_CAP as usize + 1,
        i32::MAX as usize,
        u32::MAX as usize + 1,
        usize::MAX,
    ] {
        let error = CursorLimit::from_user(raw, DEFAULT_LIMIT_CAP, "limit").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("limit must be between 1 and {DEFAULT_LIMIT_CAP}, got {raw}")
        );
    }
    // 1 and the cap are served
    for raw in [1, DEFAULT_LIMIT_CAP as usize] {
        let limit = CursorLimit::from_user(raw, DEFAULT_LIMIT_CAP, "limit").unwrap();
        assert_eq!(limit.get(), raw);
    }
    // the cap plus one still fits the int a statement binds, for both caps
    let cap = CursorLimit::from_user(DEFAULT_LIMIT_CAP as usize, DEFAULT_LIMIT_CAP, "limit");
    assert_eq!(cap.unwrap().plus_one().to_cql(), 10_001);
    let stage = CursorLimit::from_user(STAGE_LOG_LIMIT_CAP as usize, STAGE_LOG_LIMIT_CAP, "limit");
    assert_eq!(stage.unwrap().plus_one().to_cql(), 250_001);
    // no route can raise its cap past the stage-log cap
    assert!(CursorLimit::from_user(250_001, u32::MAX, "limit").is_err());
}

/// The last bucket of a year counts that year's own days, and buckets step back without
/// wrapping
#[test]
fn buckets_are_checked() {
    // 2028 is a leap year with 366 days of hour-long buckets, numbered from 0, and 2027 has 365
    assert_eq!(
        Bucket::last_of_year(2028, 3_600),
        Ok(Bucket::new(366 * 24 - 1))
    );
    assert_eq!(
        Bucket::last_of_year(2027, 3_600),
        Ok(Bucket::new(365 * 24 - 1))
    );
    // the year's last 180 second bucket is the bucket of its final second
    assert_eq!(Bucket::last_of_year(2024, 180), Ok(Bucket::new(175_679)));
    assert_eq!(Bucket::last_of_year(2025, 180), Ok(Bucket::new(175_199)));
    // the first bucket has no bucket before it
    assert_eq!(Bucket::new(0).prev(), None);
    assert_eq!(Bucket::new(7).prev(), Some(Bucket::new(6)));
    // a bucket only binds when it fits an int
    assert_eq!(Bucket::new(i32::MAX as u32).to_cql(), Ok(i32::MAX));
    assert_eq!(
        Bucket::new(i32::MAX as u32 + 1).to_cql(),
        Err(BucketError::TooLarge(i32::MAX as u32 + 1))
    );
    // a partition size of 0 is an error instead of a division by zero
    let new_year = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    assert_eq!(
        Bucket::of(new_year, 2026, 0),
        Err(BucketError::ZeroPartitionSize)
    );
    // the first second of a year is in bucket 0 for any partition size
    assert_eq!(Bucket::of(new_year, 2026, 1), Ok(Bucket::new(0)));
    assert_eq!(Bucket::of(new_year, 2026, 180), Ok(Bucket::new(0)));
    // a timestamp before its year has no bucket in it
    let before = Utc.with_ymd_and_hms(2025, 12, 31, 12, 0, 0).unwrap();
    assert!(Bucket::of(before, 2026, 180).is_err());
    // a stored bucket must be a non-negative number
    assert_eq!(Bucket::from_stored("12"), Ok(Bucket::new(12)));
    assert!(Bucket::from_stored("-1").is_err());
}

/// Every bucket a timestamp falls in is the bucket rows are written to, past the year's first
/// second
#[test]
fn buckets_match_the_written_buckets() {
    // check timestamps through a leap year at several partition sizes
    for size in [1_u16, 180, 600, 3_600, 65_535] {
        for (month, day, hour, minute, second) in [
            (1, 1, 0, 0, 1),
            (1, 1, 0, 3, 0),
            (2, 29, 12, 0, 0),
            (6, 15, 8, 30, 15),
            (12, 31, 23, 59, 59),
        ] {
            let timestamp = Utc
                .with_ymd_and_hms(2024, month, day, hour, minute, second)
                .unwrap();
            let written = helpers::partition(timestamp, 2024, size);
            assert_eq!(
                Bucket::of(timestamp, 2024, size).map(Bucket::get),
                Ok(u32::try_from(written).unwrap()),
                "{timestamp} at size {size}"
            );
        }
    }
}

/// A pull reads at most 99 partitions, so its buckets shrink as its other lists grow
#[test]
fn bucket_limits_are_checked() {
    // with no other lists a pull reads 99 buckets
    assert_eq!(bucket_limit(&[]), Ok(99));
    // three kinds leave room for 33 buckets
    assert_eq!(bucket_limit(&[3]), Ok(33));
    assert_eq!(bucket_limit(&[99]), Ok(1));
    // an empty list or one too long to leave a bucket is refused
    assert!(bucket_limit(&[0]).is_err());
    assert!(bucket_limit(&[100]).is_err());
    assert!(bucket_limit(&[usize::MAX, 2]).is_err());
}

/// Build a clustering column of a fixture table
///
/// # Arguments
///
/// * `name` - The column's name
/// * `position` - Its position in the clustering key
/// * `order` - Its declared order
const fn cluster(name: &'static str, position: u8, order: ClusterOrder) -> ColumnDef {
    ColumnDef {
        name,
        desc: CqlDesc::Native(CqlNative::Text),
        role: ColumnRole::Cluster(position, order),
        security_sensitive: false,
    }
}

/// A fixture table with every pair of clustering orders after its partition column
const FIXTURE: ObjectDef = ObjectDef::Table(TableDef {
    name: "fixture",
    columns: &[
        ColumnDef {
            name: "group",
            desc: CqlDesc::Native(CqlNative::Text),
            role: ColumnRole::Partition(0),
            security_sensitive: false,
        },
        cluster("at", 0, ClusterOrder::Desc),
        cluster("asc_tie", 1, ClusterOrder::Asc),
        cluster("asc_tie_two", 2, ClusterOrder::Asc),
    ],
    ttl: None,
    partition_size: None,
});

/// A fixture table whose range column is ascending
const ASCENDING: ObjectDef = ObjectDef::Table(TableDef {
    name: "ascending",
    columns: &[
        ColumnDef {
            name: "group",
            desc: CqlDesc::Native(CqlNative::Text),
            role: ColumnRole::Partition(0),
            security_sensitive: false,
        },
        cluster("at", 0, ClusterOrder::Asc),
        cluster("tie", 1, ClusterOrder::Asc),
    ],
    ttl: None,
    partition_size: None,
});

/// The comparison of a tie query and any `ORDER BY` follow the declared clustering order and
/// the order the cursor consumes its range column in
#[test]
fn comparisons_follow_the_clustering_order() {
    // a descending range with ascending ties reads the ties from the tie onward ascending
    assert_eq!(cursor_order(&FIXTURE, "at", None), Order::Desc);
    let order = tie_order(&FIXTURE, "at", "asc_tie", None);
    assert_eq!(order, Order::Asc);
    assert_eq!(tie_from(order), ">=");
    // a tie of two columns reads past the first column's tie, then from the whole tie onward
    let ties = ["asc_tie", "asc_tie_two"];
    assert_eq!(tie_predicates(&ties, 0, order), ["asc_tie > ?"]);
    assert_eq!(
        tie_predicates(&ties, 1, order),
        ["asc_tie = ?", "asc_tie_two >= ?"]
    );
    // consuming in the declared order needs no ORDER BY
    assert_eq!(order_by(&FIXTURE, "at", &["asc_tie"], None), None);
    // an ascending table read newest first reverses its ties and orders both columns
    let desc = Some(Order::Desc);
    assert_eq!(tie_order(&ASCENDING, "at", "tie", desc), Order::Desc);
    assert_eq!(tie_from(tie_order(&ASCENDING, "at", "tie", desc)), "<=");
    assert_eq!(
        order_by(&ASCENDING, "at", &["tie"], desc).as_deref(),
        Some("ORDER BY at DESC, tie DESC")
    );
    // a page resumes after an ascending column with `>` and a descending one with `<`
    assert_eq!(resume_after(Order::Asc), ">");
    assert_eq!(resume_after(Order::Desc), "<");
    assert_eq!(Order::Asc.reverse(), Order::Desc);
}
