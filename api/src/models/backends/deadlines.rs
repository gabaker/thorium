//! Wrappers for interacting with deadlines within Thorium with different backends
//! Currently only Redis is supported

use chrono::prelude::*;
use tracing::instrument;

use crate::models::{Deadline, ImageScaler, Stream, StreamObj, User, deadlines::DeadlineFragment};
use crate::utils::{ApiError, Shared, macros};
use crate::{bad, internal_err};

impl Deadline {
    /// Get the timestamp of this deadline in seconds (unix epoch)
    pub(super) fn as_seconds(&self) -> i64 {
        self.deadline.timestamp()
    }

    /// Read deadlines between a start and end timestamp
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reading deadlines
    /// * `scaler` - The scaler to get deadlines for
    /// * `start` - The start timestamp to read from in seconds (unix epoch)
    /// * `end` - The end timestamp to read to in seconds (unix epoch)
    /// * `skip` - The number of entries in the running stream to skip
    /// * `limit` - The most deadlines to return at once
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Deadline::read", skip(user, shared), err(Debug))]
    pub async fn read(
        user: &User,
        scaler: ImageScaler,
        start: i64,
        end: i64,
        skip: u64,
        limit: u64,
        shared: &Shared,
    ) -> Result<Vec<Self>, ApiError> {
        // get the name of scaler for this deadline
        let ns = scaler.to_string();
        // read objects from streams
        let objects = Stream::read(
            user,
            "system",
            &ns,
            "deadlines",
            start,
            end,
            skip,
            limit,
            shared,
        )
        .await?
        .into_iter()
        .map(Deadline::try_from)
        .filter_map(macros::log_err)
        .collect();
        Ok(objects)
    }

    /// Removes a deadline from the deadlines stream
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to delete a deadline for
    /// * `shared` - Shared Thorium objects
    pub async fn delete(self, scaler: ImageScaler, shared: &Shared) -> Result<(), ApiError> {
        // get the name of scaler for this deadline
        let ns = scaler.to_string();
        let obj = StreamObj::from(self);
        Stream::system_delete(&ns, "deadlines", &obj.data, shared).await?;
        Ok(())
    }
}

impl TryFrom<StreamObj> for Deadline {
    type Error = ApiError;

    /// Attempt to cast a `StreamObj` into a Deadline
    ///
    /// # Arguments
    ///
    /// * `obj` - The `StreamObj` to cast to a Deadline
    fn try_from(obj: StreamObj) -> Result<Self, Self::Error> {
        // ingest data from stream object
        let frag: DeadlineFragment = match serde_json::from_str(&obj.data) {
            Ok(frag) => frag,
            Err(e) => {
                return bad!(format!(
                    "failed to cast StreamObj to DeadlineFragment {}",
                    e
                ));
            }
        };
        // convert to DateTime
        let datetime = match DateTime::from_timestamp(obj.timestamp, 0) {
            Some(deadline) => deadline,
            None => return internal_err!(format!("{} is not a valid timestamp", obj.timestamp)),
        };
        // cast to deadline
        let deadline = Deadline {
            group: frag.group,
            pipeline: frag.pipeline,
            stage: frag.stage,
            creator: frag.creator,
            job_id: frag.job_id,
            reaction: frag.reaction,
            deadline: datetime,
        };
        Ok(deadline)
    }
}
