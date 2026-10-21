use crate::utils::ApiError;

impl From<bb8_redis::redis::RedisError> for ApiError {
    /// Cast a bb8_redis error into an ApiError
    ///
    /// # Arguments
    ///
    /// * `error` - The bb8_redis error to convert to an ApiError
    fn from(error: bb8_redis::redis::RedisError) -> Self {
        ApiError::Redis(error)
    }
}

impl From<scylla::errors::ExecutionError> for ApiError {
    /// Cast a scylla error into an ApiError
    ///
    /// # Arguments
    ///
    /// * `error` - The scylla error to convert to an ApiError
    fn from(error: scylla::errors::ExecutionError) -> Self {
        ApiError::ScyllaExecution(Box::new(error))
    }
}

impl From<crate::models::schema::QueryError> for ApiError {
    /// Cast a generated statement's error into the `ApiError` its driver error maps to
    ///
    /// # Arguments
    ///
    /// * `error` - The generated statement's error
    fn from(error: crate::models::schema::QueryError) -> Self {
        use crate::models::schema::QueryError;
        // each driver error maps the way it does when a call site runs the statement itself
        match error {
            QueryError::Execution(error) => ApiError::from(*error),
            QueryError::IntoRows(error) => ApiError::from(*error),
            QueryError::Rows(error) => ApiError::from(error),
            QueryError::FirstRow(error) => ApiError::from(error),
            QueryError::Pager(error) => ApiError::from(*error),
            QueryError::TypeCheck(error) => ApiError::from(error),
            QueryError::Bucket(error) => ApiError::from(error),
        }
    }
}

impl From<crate::models::schema::LimitError> for ApiError {
    /// Cast a limit a route can't serve into a 400 naming the limits it allows
    ///
    /// # Arguments
    ///
    /// * `error` - The limit's error
    fn from(error: crate::models::schema::LimitError) -> Self {
        ApiError::new(axum::http::StatusCode::BAD_REQUEST, Some(error.to_string()))
    }
}

impl From<crate::models::schema::BucketLimitError> for ApiError {
    /// Cast filters a listing can't serve into a 400 naming the sizes it allows
    ///
    /// # Arguments
    ///
    /// * `error` - The filters' error
    fn from(error: crate::models::schema::BucketLimitError) -> Self {
        ApiError::new(axum::http::StatusCode::BAD_REQUEST, Some(error.to_string()))
    }
}

impl From<crate::models::schema::BucketError> for ApiError {
    /// Cast a bucket that can't be computed or doesn't fit into a 500, since buckets come from
    /// stored data or the config
    ///
    /// # Arguments
    ///
    /// * `error` - The bucket's error
    fn from(error: crate::models::schema::BucketError) -> Self {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Some(error.to_string()),
        )
    }
}
