//! Runtime support for generated cursor statements: the statements trait of each cursor family,
//! the order cursors consume their clustering columns in, and the helpers that render and
//! compare those columns

use chrono::{DateTime, Utc};
use scylla::client::pager::TypedRowStream;
use scylla::client::session::Session;
use scylla::deserialize::row::DeserializeRow;
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::CellWriter;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::cmp::Ordering;
use std::fmt::{Debug, Display};
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;

use super::checks::find_column;
use super::cql_type::{CqlDesc, CqlNative};
use super::definition::{ClusterOrder, ColumnAt, ColumnRole, ObjectDef};
use super::queries_runtime::{Bucket, CursorLimit, QueryError, Restriction, RowResult};

/// The future a cursor statement's method returns, boxed so the futures of generic cursor code
/// stay `Send` whatever lifetimes their arguments borrow
pub type CursorFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, QueryError>> + Send + 'a>>;

/// The order a cursor consumes a clustering column in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Oldest or smallest first
    Asc,
    /// Newest or largest first
    Desc,
}

impl Order {
    /// The opposite order
    #[must_use]
    pub const fn reverse(self) -> Self {
        match self {
            Order::Asc => Order::Desc,
            Order::Desc => Order::Asc,
        }
    }

    /// The order a clustering column is declared with
    ///
    /// # Arguments
    ///
    /// * `order` - The declared clustering order
    #[must_use]
    pub const fn of(order: ClusterOrder) -> Self {
        match order {
            ClusterOrder::Asc => Order::Asc,
            ClusterOrder::Desc => Order::Desc,
        }
    }

    /// Whether two orders are the same, in a const context
    ///
    /// # Arguments
    ///
    /// * `other` - The order to compare with
    #[must_use]
    pub const fn same(self, other: Order) -> bool {
        matches!(
            (self, other),
            (Order::Asc, Order::Asc) | (Order::Desc, Order::Desc)
        )
    }

    /// The CQL keyword for this order
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        }
    }
}

/// A list of values filtering a cursor's partition key that its pulls can't serve
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketLimitError {
    /// The number of values in each `IN` list the cursor binds besides its buckets
    pub lens: Vec<usize>,
}

impl std::fmt::Display for BucketLimitError {
    /// Say which list sizes a cursor serves
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a listing filters by between 1 and {} values, got {:?}",
            PULL_PRODUCT_LIMIT, self.lens
        )
    }
}

impl std::error::Error for BucketLimitError {}

/// The most partitions one pull reads: its buckets times the values of its other `IN` lists
pub const PULL_PRODUCT_LIMIT: usize = 99;

/// The most buckets one pull reads so that, with the cursor's other `IN` lists, it reads at
/// most [`PULL_PRODUCT_LIMIT`] partitions
///
/// # Arguments
///
/// * `lens` - The number of values in each of the cursor's other `IN` lists
pub fn bucket_limit(lens: &[usize]) -> Result<u32, BucketLimitError> {
    // the lists multiply into the partitions each bucket reads
    let product = lens
        .iter()
        .try_fold(1_usize, |product, len| product.checked_mul(*len));
    // an empty list reads nothing, and a product above the limit leaves no room for buckets
    match product {
        Some(product) if product > 0 && product <= PULL_PRODUCT_LIMIT => {
            u32::try_from(PULL_PRODUCT_LIMIT / product).map_err(|_| BucketLimitError {
                lens: lens.to_vec(),
            })
        }
        _ => Err(BucketLimitError {
            lens: lens.to_vec(),
        }),
    }
}

/// The statements of a time-bucket cursor, generated from `#[cursor(kind = time, ...)]`
///
/// A pull reads a window of buckets of one group between two timestamps, newest first, and a
/// tie query reads the rows of one partition at a timestamp from a tie onward, so a page that
/// ends inside rows sharing a timestamp resumes where it stopped.
pub trait TimeCursorStatements: Send + Sync {
    /// The filters bound in both statements besides the group, such as the kinds and repo of a
    /// commitish listing
    type Extra: Debug + Serialize + DeserializeOwned + Send + Sync;
    /// The partition column a listing fans out over, one pull per value
    type Group: Display + Debug + Clone + Hash + Eq + Serialize + DeserializeOwned + Send + Sync;
    /// The projection both statements read
    type Row: for<'f, 'm> DeserializeRow<'f, 'm> + Send;
    /// The clustering columns after the range column, which break ties within a timestamp
    type Tie: Debug + Clone + PartialEq + Serialize + DeserializeOwned + Send + Sync;
    /// The partition a tie is recorded for, without its year and bucket: the group and the
    /// value of each `IN` filter
    type TieScope: Debug + Clone + PartialEq + Serialize + DeserializeOwned + Send + Sync;

    /// The order the statements return their range column in
    const ORDER: Order;

    /// The most buckets one pull reads for these filters
    ///
    /// # Arguments
    ///
    /// * `extra` - The listing's filters
    fn bucket_limit(extra: &Self::Extra) -> Result<u32, BucketLimitError>;

    /// Read the rows of some buckets of one group strictly between two timestamps
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `extra` - The listing's filters
    /// * `group` - The group to read
    /// * `year` - The year of the buckets
    /// * `buckets` - The buckets to read
    /// * `before` - Read rows older than this
    /// * `after` - Read rows newer than this
    /// * `limit` - The most rows to read from each partition
    #[allow(clippy::too_many_arguments)]
    fn pull<'a>(
        &'a self,
        session: &'a Session,
        extra: &'a Self::Extra,
        group: &'a Self::Group,
        year: i32,
        buckets: &'a [Bucket],
        before: DateTime<Utc>,
        after: DateTime<Utc>,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// Read the rows of one partition at a timestamp from a tie onward, in consumption order
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `extra` - The listing's filters
    /// * `scope` - The partition to read, without its year and bucket
    /// * `year` - The partition's year
    /// * `bucket` - The partition's bucket
    /// * `at` - The timestamp the rows share
    /// * `tie` - The first row to read
    /// * `limit` - The most rows to read
    #[allow(clippy::too_many_arguments)]
    fn ties<'a>(
        &'a self,
        session: &'a Session,
        extra: &'a Self::Extra,
        scope: &'a Self::TieScope,
        year: i32,
        bucket: Bucket,
        at: DateTime<Utc>,
        tie: &'a Self::Tie,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// The timestamp of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_time(row: &Self::Row) -> DateTime<Utc>;

    /// The tie of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_tie(row: &Self::Row) -> Self::Tie;

    /// The partition a row was read from, without its year and bucket
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_scope(row: &Self::Row) -> Self::TieScope;

    /// Compare two ties in the order a page consumes rows sharing a timestamp, the order the
    /// tie query reads them within one partition
    ///
    /// # Arguments
    ///
    /// * `left` - The first tie
    /// * `right` - The second tie
    fn cmp_ties(left: &Self::Tie, right: &Self::Tie) -> Ordering;

    /// Compare two scopes in the order of their partition key columns
    ///
    /// # Arguments
    ///
    /// * `left` - The first scope
    /// * `right` - The second scope
    fn cmp_scopes(left: &Self::TieScope, right: &Self::TieScope) -> Ordering;

    /// The group of the partition a tie is recorded for
    ///
    /// # Arguments
    ///
    /// * `scope` - The partition, without its year and bucket
    fn scope_group(scope: &Self::TieScope) -> &Self::Group;
}

/// The statements of a time-bucket cursor read through a pager without a limit, generated from
/// `#[cursor(kind = time, paged, ...)]`; tag listings read every matching row of a window
pub trait PagedTimeCursorStatements: Send + Sync {
    /// The filters bound in both statements besides the group
    type Extra: Debug + Send + Sync;
    /// The partition column a listing fans out over
    type Group: Display + Debug + Clone + Hash + Eq + Serialize + DeserializeOwned + Send + Sync;
    /// The projection both statements read
    type Row: for<'f, 'm> DeserializeRow<'f, 'm> + Send + 'static;
    /// The clustering columns after the range column
    type Tie: Debug + Clone + PartialEq + Serialize + DeserializeOwned + Send + Sync;

    /// Page through the rows of some buckets of one group strictly between two timestamps
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `extra` - The listing's filters
    /// * `group` - The group to read
    /// * `year` - The year of the buckets
    /// * `buckets` - The buckets to read
    /// * `before` - Read rows older than this
    /// * `after` - Read rows newer than this
    #[allow(clippy::too_many_arguments)]
    fn pull<'a>(
        &'a self,
        session: &'a Session,
        extra: &'a Self::Extra,
        group: &'a Self::Group,
        year: i32,
        buckets: &'a [Bucket],
        before: DateTime<Utc>,
        after: DateTime<Utc>,
    ) -> CursorFuture<'a, TypedRowStream<Self::Row>>;

    /// Page through the rows of one group's bucket at a timestamp from a tie onward
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `extra` - The listing's filters
    /// * `group` - The group to read
    /// * `year` - The bucket's year
    /// * `bucket` - The bucket
    /// * `at` - The timestamp the rows share
    /// * `tie` - The first row to read
    #[allow(clippy::too_many_arguments)]
    fn ties<'a>(
        &'a self,
        session: &'a Session,
        extra: &'a Self::Extra,
        group: &'a Self::Group,
        year: i32,
        bucket: Bucket,
        at: DateTime<Utc>,
        tie: &'a Self::Tie,
    ) -> CursorFuture<'a, TypedRowStream<Self::Row>>;

    /// The timestamp of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_time(row: &Self::Row) -> DateTime<Utc>;

    /// The tie of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_tie(row: &Self::Row) -> Self::Tie;
}

/// The statements of a cursor over the clustering key of one partition at a time, generated from
/// `#[cursor(kind = simple, ...)]`
pub trait SimpleCursorStatements: Send + Sync {
    /// The partition column the cursor lists within
    type Partition: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;
    /// The projection both statements read
    type Row: for<'f, 'm> DeserializeRow<'f, 'm> + Send;
    /// The clustering column a page resumes after
    type Tie: Debug + Clone + Serialize + DeserializeOwned + Send + Sync;

    /// Read the first rows of a partition
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `partition` - The partition to read
    /// * `limit` - The most rows to read
    fn first<'a>(
        &'a self,
        session: &'a Session,
        partition: &'a Self::Partition,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// Read the rows of a partition after a tie
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `partition` - The partition to read
    /// * `tie` - The last row already read
    /// * `limit` - The most rows to read
    fn after<'a>(
        &'a self,
        session: &'a Session,
        partition: &'a Self::Partition,
        tie: &'a Self::Tie,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// The tie of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_tie(row: &Self::Row) -> Self::Tie;
}

/// The statements of a cursor over groups sorted by a clustering column, generated from
/// `#[cursor(kind = grouped, ...)]`
pub trait GroupedCursorStatements: Send + Sync {
    /// The partition column each group is read from
    type Group: Debug + Clone + Hash + Eq + Serialize + DeserializeOwned + Send + Sync;
    /// The clustering column rows are sorted by
    type SortBy: Debug + Clone + Ord + Serialize + DeserializeOwned + Send + Sync;
    /// The column that tells apart rows of different items sharing a sort value
    type TieBreaker: Eq;
    /// The projection the statements read
    type Row: for<'f, 'm> DeserializeRow<'f, 'm> + Send;

    /// Read the first rows of a group
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `group` - The group to read
    /// * `limit` - The most rows to read
    fn pull<'a>(
        &'a self,
        session: &'a Session,
        group: &'a Self::Group,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// Read the rows of a group sorted after a value
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `group` - The group to read
    /// * `after` - The last sort value already read
    /// * `limit` - The most rows to read
    fn pull_more<'a>(
        &'a self,
        session: &'a Session,
        group: &'a Self::Group,
        after: &'a Self::SortBy,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// Read the rows of a group with a sort value
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `group` - The group to read
    /// * `at` - The sort value
    /// * `limit` - The most rows to read
    fn ties<'a>(
        &'a self,
        session: &'a Session,
        group: &'a Self::Group,
        at: &'a Self::SortBy,
        limit: CursorLimit,
    ) -> CursorFuture<'a, Vec<RowResult<Self::Row>>>;

    /// The group of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_group(row: &Self::Row) -> Self::Group;

    /// The sort value of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_sort(row: &Self::Row) -> Self::SortBy;

    /// The tie breaker of a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row
    fn row_tie_breaker(row: &Self::Row) -> &Self::TieBreaker;
}

/// A statement checking whether any row with a key exists in some buckets of a year, generated
/// from `#[cursor(kind = exists, ...)]`
pub trait ExistsStatement: Send + Sync {
    /// The key column checked
    type Key: Sync + ?Sized;

    /// Whether any row with this key exists in these buckets
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session to execute on
    /// * `year` - The year of the buckets
    /// * `buckets` - The buckets to check
    /// * `key` - The key to look for
    fn any<'a>(
        &'a self,
        session: &'a Session,
        year: i32,
        buckets: &'a [Bucket],
        key: &'a Self::Key,
    ) -> CursorFuture<'a, bool>;
}

/// The declared order of a clustering column; other columns count as ascending
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `column` - The column's name
#[must_use]
pub const fn declared_order(object: &ObjectDef, column: &str) -> Order {
    match find_column(object, column) {
        Some(def) => match def.role {
            ColumnRole::Cluster(_, order) => Order::of(order),
            _ => Order::Asc,
        },
        None => Order::Asc,
    }
}

/// The order a time cursor consumes its range column in: the order it names, or the range
/// column's declared order
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `range` - The range column
/// * `order` - The consumption order the cursor names, if any
#[must_use]
pub const fn cursor_order(object: &ObjectDef, range: &str, order: Option<Order>) -> Order {
    match order {
        Some(order) => order,
        None => declared_order(object, range),
    }
}

/// The order a cursor consumes one of its tie columns in: its declared order, reversed when the
/// cursor consumes its range column against the range column's declared order
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `range` - The range column
/// * `tie` - The tie column
/// * `order` - The consumption order the cursor names, if any
#[must_use]
pub const fn tie_order(object: &ObjectDef, range: &str, tie: &str, order: Option<Order>) -> Order {
    let declared = declared_order(object, tie);
    // reading the range column reversed reads every later clustering column reversed too
    if cursor_order(object, range, order).same(declared_order(object, range)) {
        declared
    } else {
        declared.reverse()
    }
}

/// The `ORDER BY` clause a time cursor's statements need, when the cursor consumes its range
/// column against its declared order
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `range` - The range column
/// * `ties` - The tie columns
/// * `order` - The consumption order the cursor names, if any
#[must_use]
pub fn order_by(
    object: &ObjectDef,
    range: &str,
    ties: &[&str],
    order: Option<Order>,
) -> Option<String> {
    let consumed = cursor_order(object, range, order);
    // the declared order needs no clause
    if consumed.same(declared_order(object, range)) {
        return None;
    }
    // name the range column and each tie column in the order they're consumed
    let columns = std::iter::once(format!("{range} {}", consumed.keyword()))
        .chain(
            ties.iter()
                .map(|tie| format!("{tie} {}", tie_order(object, range, tie, order).keyword())),
        )
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("ORDER BY {columns}"))
}

/// The comparison a tie query reads its tie columns from the tie onward with: `>=` when they're
/// consumed ascending and `<=` when descending
///
/// # Arguments
///
/// * `order` - The order the tie columns are consumed in
#[must_use]
pub const fn tie_from(order: Order) -> &'static str {
    match order {
        Order::Asc => ">=",
        Order::Desc => "<=",
    }
}

/// The comparison a page resumes after a clustering column with: `>` when it's consumed
/// ascending and `<` when descending
///
/// # Arguments
///
/// * `order` - The order the column is consumed in
#[must_use]
pub const fn resume_after(order: Order) -> &'static str {
    match order {
        Order::Asc => ">",
        Order::Desc => "<",
    }
}

/// The limit a cursor statement ends with
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorLimitClause {
    /// `PER PARTITION LIMIT ?`
    PerPartition,
    /// `LIMIT ?`
    Limit,
    /// A literal `PER PARTITION LIMIT 1`
    PerPartitionOne,
    /// No limit
    None,
}

/// Render a cursor statement
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `object` - The table or view read
/// * `columns` - The selected columns
/// * `restrictions` - The restricted partition key columns in key order, with how each is
///   restricted
/// * `clustering` - The predicates on clustering columns, in order
/// * `order_by` - The `ORDER BY` clause, if the statement needs one
/// * `limit` - The limit the statement ends with
#[must_use]
pub fn cursor_cql(
    ns: &str,
    object: &str,
    columns: &[&str],
    restrictions: &[(&str, Restriction)],
    clustering: &[String],
    order_by: Option<&str>,
    limit: CursorLimitClause,
) -> String {
    // the partition key restrictions come first, then the clustering predicates
    let predicates = restrictions
        .iter()
        .map(|(column, restriction)| match restriction {
            Restriction::In => format!("{column} IN ?"),
            _ => format!("{column} = ?"),
        })
        .chain(clustering.iter().cloned())
        .collect::<Vec<_>>()
        .join(" AND ");
    let mut cql = format!(
        "SELECT {} FROM {ns}.{object} WHERE {predicates}",
        columns.join(", ")
    );
    // add the trailing clauses in the order CQL requires
    if let Some(order_by) = order_by {
        cql.push(' ');
        cql.push_str(order_by);
    }
    match limit {
        CursorLimitClause::PerPartition => cql.push_str(" PER PARTITION LIMIT ?"),
        CursorLimitClause::Limit => cql.push_str(" LIMIT ?"),
        CursorLimitClause::PerPartitionOne => cql.push_str(" PER PARTITION LIMIT 1"),
        CursorLimitClause::None => {}
    }
    cql
}

/// Render the tie predicates of one of a time cursor's tie queries: the tie columns before
/// `index` equal their values, and the column at `index` compared with its value, strictly
/// unless it's the last tie column
///
/// Scylla refuses a multi-column comparison that skips the range column, and one that includes
/// it would mix clustering orders, so a tie of several columns is read with one query per
/// column, each fixing the columns before its own.
///
/// # Arguments
///
/// * `ties` - The tie columns, in clustering order
/// * `index` - The tie column this query compares
/// * `order` - The order the tie columns are consumed in
#[must_use]
pub fn tie_predicates(ties: &[&str], index: usize, order: Order) -> Vec<String> {
    // the earlier tie columns hold their values
    let mut predicates = ties[..index]
        .iter()
        .map(|tie| format!("{tie} = ?"))
        .collect::<Vec<_>>();
    // the last tie column reads from the tie onward, and earlier ones read past it
    let op = if index + 1 == ties.len() {
        tie_from(order)
    } else {
        resume_after(order)
    };
    predicates.push(format!("{} {op} ?", ties[index]));
    predicates
}

/// The CQL type a scalar column binds as
///
/// # Arguments
///
/// * `native` - The column's scalar type
fn native_type(native: CqlNative) -> ColumnType<'static> {
    ColumnType::Native(match native {
        CqlNative::Text => NativeType::Text,
        CqlNative::TinyInt => NativeType::TinyInt,
        CqlNative::SmallInt => NativeType::SmallInt,
        CqlNative::Int => NativeType::Int,
        CqlNative::BigInt => NativeType::BigInt,
        CqlNative::Boolean => NativeType::Boolean,
        CqlNative::Float => NativeType::Float,
        CqlNative::Double => NativeType::Double,
        CqlNative::Uuid => NativeType::Uuid,
        CqlNative::Timestamp => NativeType::Timestamp,
    })
}

/// The bytes a value is stored as in a column, or `None` for a null or a value that fails to
/// serialize
///
/// # Arguments
///
/// * `value` - The value
/// * `typ` - The column's CQL type
fn stored_bytes<O: ColumnAt<N>, const N: usize>(
    value: &O::Owned,
    typ: &ColumnType<'_>,
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    O::serialize_owned(value, typ, CellWriter::new(&mut buf)).ok()?;
    // a cell is its length as a four byte integer and then its bytes; a null has length -1
    let (len, bytes) = buf.split_first_chunk::<4>()?;
    (i32::from_be_bytes(*len) >= 0).then(|| bytes.to_vec())
}

/// Compare two stored values of a scalar type the way Scylla orders them in a clustering column
///
/// # Arguments
///
/// * `native` - The column's scalar type
/// * `left` - The first value's bytes
/// * `right` - The second value's bytes
fn cmp_stored(native: CqlNative, left: &[u8], right: &[u8]) -> Ordering {
    match native {
        // text sorts by its UTF-8 bytes and booleans by their byte
        CqlNative::Text | CqlNative::Boolean => left.cmp(right),
        // uuids sort by version first and then by their bytes
        CqlNative::Uuid => {
            let version = |bytes: &[u8]| bytes.get(6).map(|byte| byte >> 4);
            version(left)
                .cmp(&version(right))
                .then_with(|| left.cmp(right))
        }
        // integers and timestamps are signed big-endian, so flipping the sign bit sorts them
        CqlNative::TinyInt
        | CqlNative::SmallInt
        | CqlNative::Int
        | CqlNative::BigInt
        | CqlNative::Timestamp => {
            let flip = |bytes: &[u8]| {
                let mut flipped = bytes.to_vec();
                if let Some(first) = flipped.first_mut() {
                    *first ^= 0x80;
                }
                flipped
            };
            flip(left).cmp(&flip(right))
        }
        // floats compare by value
        CqlNative::Float => {
            let value = |bytes: &[u8]| bytes.try_into().map_or(f32::NAN, f32::from_be_bytes);
            value(left)
                .partial_cmp(&value(right))
                .unwrap_or(Ordering::Equal)
        }
        CqlNative::Double => {
            let value = |bytes: &[u8]| bytes.try_into().map_or(f64::NAN, f64::from_be_bytes);
            value(left)
                .partial_cmp(&value(right))
                .unwrap_or(Ordering::Equal)
        }
    }
}

/// Compare two values of a column the way Scylla orders the column ascending: by their stored
/// bytes, so a type stored as text sorts by that text
///
/// # Arguments
///
/// * `left` - The first value
/// * `right` - The second value
#[must_use]
pub fn cmp_column<O: ColumnAt<N>, const N: usize>(left: &O::Owned, right: &O::Owned) -> Ordering {
    // only scalar columns are clustering or partition columns
    let Some(CqlDesc::Native(native)) = find_column(&O::DEF, O::COLUMN).map(|column| column.desc)
    else {
        return Ordering::Equal;
    };
    let typ = native_type(native);
    // a null sorts first
    match (
        stored_bytes::<O, N>(left, &typ),
        stored_bytes::<O, N>(right, &typ),
    ) {
        (Some(left), Some(right)) => cmp_stored(native, &left, &right),
        (left, right) => left.is_some().cmp(&right.is_some()),
    }
}

/// Compare two values of a column in the order a cursor consumes the column
///
/// # Arguments
///
/// * `left` - The first value
/// * `right` - The second value
/// * `order` - The order the column is consumed in
#[must_use]
pub fn cmp_consumed<O: ColumnAt<N>, const N: usize>(
    left: &O::Owned,
    right: &O::Owned,
    order: Order,
) -> Ordering {
    match order {
        Order::Asc => cmp_column::<O, N>(left, right),
        Order::Desc => cmp_column::<O, N>(right, left),
    }
}

/// One value bound to a column, given as the column's own type and written the way the column's
/// object writes it
pub struct BindOwned<'v, O: ColumnAt<N>, const N: usize>(
    /// The value
    pub &'v O::Owned,
);

impl<O: ColumnAt<N>, const N: usize> SerializeValue for BindOwned<'_, O, N> {
    /// Write the value through its column's conversion
    ///
    /// # Arguments
    ///
    /// * `typ` - The bind marker's CQL type
    /// * `writer` - The writer for the value
    fn serialize<'w>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'w>,
    ) -> Result<
        scylla::serialize::writers::WrittenCellProof<'w>,
        scylla::serialize::SerializationError,
    > {
        O::serialize_owned(self.0, typ, writer)
    }
}

/// Convert buckets into the CQL `int`s their column binds
///
/// # Arguments
///
/// * `buckets` - The buckets
pub fn bucket_ints(buckets: &[Bucket]) -> Result<Vec<i32>, QueryError> {
    buckets
        .iter()
        .map(|bucket| bucket.to_cql().map_err(QueryError::from))
        .collect()
}
