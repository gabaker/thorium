//! The errors that may occur during backup or restore

use bytecheck::{SliceCheckError, StructCheckError};
use rkyv::validation::{
    CheckArchiveError, owned::OwnedPointerError, validators::DefaultValidatorError,
};
use std::convert::Infallible;

#[derive(Debug)]
pub enum Error {
    /// A generic backup error
    Generic(String),
    /// A Thorium API error
    Thorium(thorium::Error),
    /// A Scylla new session error occured
    ScyllaNewSession(scylla::errors::NewSessionError),
    /// An error preparing a scylla statement
    ScyllaPrepareSatement(scylla::errors::PrepareError),
    /// A Scylla query error occured
    ScyllaQuery(scylla::errors::ExecutionError),
    /// A Scylla paged query error occured
    ScyllaPagedQuery(scylla::errors::PagerExecutionError),
    /// A Scylla next row error occured
    ScyllaNextRow(scylla::client::pager::NextRowError),
    /// A Redis error
    Redis(redis::RedisError),
    /// A tokio join error
    TokioJoin(tokio::task::JoinError),
    /// A kanal send error
    KanalSend(kanal::SendError),
    /// A kanal close error
    KanalClose(kanal::CloseError),
    /// An IO Error
    IO(std::io::Error),
    /// A config error
    Config(config::ConfigError),
    /// A deserialization/conversion error
    Conversion(std::convert::Infallible),
    /// An error from converting a type to a Uuid
    Uuid(uuid::Error),
    /// An error from converting a value with `serde_json`
    SerdeJson(serde_json::Error),
    /// An error from converting a value with `serde_norway`
    SerdeYaml(serde_norway::Error),
    /// An s3 error
    S3 {
        /// The HTTP status S3 answered with, or `None` when no response arrived
        status: Option<u16>,
        /// The S3 error code from the response body
        code: Option<String>,
        /// The S3 error message from the response body
        message: Option<String>,
        /// Whether the request was sent and no response came back: a timeout or a failed
        /// connection, rather than a request that couldn't be built
        no_response: bool,
    },
    /// An s3 bytestream error
    S3ByteStream(aws_sdk_s3::primitives::ByteStreamError),
    /// A backup file that is damaged at an offset
    Corruption {
        /// The damaged file
        path: std::path::PathBuf,
        /// The byte offset where the damage was found
        offset: u64,
        /// What is wrong there
        problem: String,
    },
    /// An S3 object's body ended before the length its response announced
    ShortBody {
        /// The length the response announced
        expected: u64,
        /// How many bytes arrived
        received: u64,
    },
    /// Failed to recieve data from a kanal channel
    KanalRecv(kanal::ReceiveError),
    /// An error with deserializing rkyv data
    RkyvDesererialize(String),
    /// An error from stripping a prefix from a path
    StripPrefix(std::path::StripPrefixError),
    // An error from dialoguer
    Dialoguer(dialoguer::Error),
    /// Every worker of a step ended early, or some did with orders left; each worker's error is
    /// reported on its own, so this error isn't recorded again
    WorkersEnded(String),
    /// The requested operation exists but is not implemented yet
    NotImplemented(String),
}

impl Error {
    /// Create a new generic error
    ///
    /// # Arguments
    ///
    /// * `msg` - The error message to use
    pub fn new<T: Into<String>>(msg: T) -> Self {
        Error::Generic(msg.into())
    }

    /// The exit code thoradm returns for this error
    ///
    /// Operations that aren't implemented yet exit with 3 so scripts can tell them apart from
    /// failures; everything else exits with 1.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        // unimplemented work gets its own code so scripts can fall back to a manual procedure
        match self {
            Error::NotImplemented(_) => 3,
            _ => crate::outcome::RunOutcome::Aborted.code(),
        }
    }

    /// Returns true when S3 answered that the object isn't there: a 404 for a key, not a bucket
    #[must_use]
    pub fn is_missing_object(&self) -> bool {
        // a missing bucket is also a 404, but it is a misconfiguration, not a broken link
        matches!(
            self,
            Error::S3 { status: Some(404), code, .. } if code.as_deref() != Some("NoSuchBucket")
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Generic(err) => write!(f, "{err}"),
            Error::Thorium(err) => write!(f, "Thorium Client Error: {err}"),
            Error::ScyllaNewSession(err) => write!(f, "ScyllaNewSession Error: {err}"),
            Error::ScyllaPrepareSatement(err) => write!(f, "ScyllaPreparedSatement Error: {err}"),
            Error::ScyllaQuery(err) => write!(f, "ScyllaQuery Error: {err}"),
            Error::ScyllaPagedQuery(err) => write!(f, "ScyllaPagedQuery Error: {err}"),
            Error::ScyllaNextRow(err) => write!(f, "ScyllaNextRow Error: {err}"),
            Error::Redis(err) => write!(f, "Redis Error: {err}"),
            Error::TokioJoin(err) => write!(f, "TokioJoin Error: {err}"),
            Error::KanalSend(err) => write!(f, "KanalSend Error: {err}"),
            Error::KanalClose(err) => write!(f, "KanalClose Error: {err}"),
            Error::IO(err) => write!(f, "IO Error: {err}"),
            Error::Config(err) => write!(f, "Config Error: {err}"),
            Error::Conversion(err) => write!(f, "Conversion Error: {err}"),
            Error::Uuid(err) => write!(f, "Uuid Error: {err}"),
            Error::SerdeJson(err) => write!(f, "SerdeJson Error: {err}"),
            Error::SerdeYaml(err) => write!(f, "SerdeYaml Error: {err}"),
            Error::S3 {
                status,
                code,
                message,
                ..
            } => {
                // name the status when S3 answered, so a 404 reads differently from a timeout
                let status = status.map_or_else(|| "no response".to_owned(), |s| s.to_string());
                write!(
                    f,
                    "S3 Error ({status}) {}: {}",
                    code.clone().unwrap_or_default(),
                    message.clone().unwrap_or_default()
                )
            }
            Error::S3ByteStream(err) => write!(f, "S3ByteStream Error: {err}"),
            Error::Corruption {
                path,
                offset,
                problem,
            } => write!(
                f,
                "{} is corrupt at byte {offset}: {problem}",
                path.display()
            ),
            Error::ShortBody { expected, received } => {
                write!(f, "S3 body ended after {received} of {expected} bytes")
            }
            Error::KanalRecv(err) => write!(f, "KanalRecv Error: {err}"),
            Error::RkyvDesererialize(err) => write!(f, "RkyvDeserialize Error: {err}"),
            Error::StripPrefix(err) => write!(f, "StripPrefix Error: {err}"),
            Error::Dialoguer(err) => write!(f, "Dialoguer: {err}"),
            Error::WorkersEnded(msg) => write!(f, "{msg}"),
            Error::NotImplemented(err) => write!(f, "Not implemented: {err}"),
        }
    }
}

impl From<thorium::Error> for Error {
    fn from(error: thorium::Error) -> Self {
        Error::Thorium(error)
    }
}

impl From<scylla::errors::NewSessionError> for Error {
    fn from(error: scylla::errors::NewSessionError) -> Self {
        Error::ScyllaNewSession(error)
    }
}

impl From<scylla::errors::PrepareError> for Error {
    fn from(error: scylla::errors::PrepareError) -> Self {
        Error::ScyllaPrepareSatement(error)
    }
}

impl From<scylla::errors::ExecutionError> for Error {
    fn from(error: scylla::errors::ExecutionError) -> Self {
        Error::ScyllaQuery(error)
    }
}

impl From<scylla::errors::PagerExecutionError> for Error {
    fn from(error: scylla::errors::PagerExecutionError) -> Self {
        Error::ScyllaPagedQuery(error)
    }
}

impl From<scylla::client::pager::NextRowError> for Error {
    fn from(error: scylla::client::pager::NextRowError) -> Self {
        Error::ScyllaNextRow(error)
    }
}

impl From<redis::RedisError> for Error {
    fn from(error: redis::RedisError) -> Self {
        Error::Redis(error)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(error: tokio::task::JoinError) -> Self {
        Error::TokioJoin(error)
    }
}

impl From<kanal::SendError> for Error {
    fn from(error: kanal::SendError) -> Self {
        Error::KanalSend(error)
    }
}

impl From<kanal::CloseError> for Error {
    fn from(error: kanal::CloseError) -> Self {
        Error::KanalClose(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::IO(error)
    }
}

impl From<config::ConfigError> for Error {
    fn from(error: config::ConfigError) -> Self {
        Error::Config(error)
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(error: std::convert::Infallible) -> Self {
        Error::Conversion(error)
    }
}

impl From<uuid::Error> for Error {
    fn from(error: uuid::Error) -> Self {
        Error::Uuid(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::SerdeJson(error)
    }
}

impl From<serde_norway::Error> for Error {
    fn from(error: serde_norway::Error) -> Self {
        Error::SerdeYaml(error)
    }
}

impl
    From<
        aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::create_multipart_upload::CreateMultipartUploadError,
        >,
    > for Error
{
    fn from(
        error: aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::create_multipart_upload::CreateMultipartUploadError,
        >,
    ) -> Self {
        // keep the HTTP status, which is gone once this becomes a service error
        let status = error
            .raw_response()
            .map(|response| response.status().as_u16());
        // a timeout or a failed connection never got an answer
        let no_response = matches!(
            error,
            aws_sdk_s3::error::SdkError::TimeoutError(_)
                | aws_sdk_s3::error::SdkError::DispatchFailure(_)
        );
        // cast this error into a service error
        let service_error = error.into_service_error();
        // get this errors metadata
        let meta = service_error.meta();
        Error::S3 {
            status,
            code: meta.code().map(ToOwned::to_owned),
            message: meta.message().map(ToOwned::to_owned),
            no_response,
        }
    }
}

impl From<aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::upload_part::UploadPartError>>
    for Error
{
    fn from(
        error: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::upload_part::UploadPartError>,
    ) -> Self {
        // keep the HTTP status, which is gone once this becomes a service error
        let status = error
            .raw_response()
            .map(|response| response.status().as_u16());
        // a timeout or a failed connection never got an answer
        let no_response = matches!(
            error,
            aws_sdk_s3::error::SdkError::TimeoutError(_)
                | aws_sdk_s3::error::SdkError::DispatchFailure(_)
        );
        // cast this error into a service error
        let service_error = error.into_service_error();
        // get this errors metadata
        let meta = service_error.meta();
        Error::S3 {
            status,
            code: meta.code().map(ToOwned::to_owned),
            message: meta.message().map(ToOwned::to_owned),
            no_response,
        }
    }
}

impl
    From<
        aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadError,
        >,
    > for Error
{
    fn from(
        error: aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::complete_multipart_upload::CompleteMultipartUploadError,
        >,
    ) -> Self {
        // keep the HTTP status, which is gone once this becomes a service error
        let status = error
            .raw_response()
            .map(|response| response.status().as_u16());
        // a timeout or a failed connection never got an answer
        let no_response = matches!(
            error,
            aws_sdk_s3::error::SdkError::TimeoutError(_)
                | aws_sdk_s3::error::SdkError::DispatchFailure(_)
        );
        // cast this error into a service error
        let service_error = error.into_service_error();
        // get this errors metadata
        let meta = service_error.meta();
        Error::S3 {
            status,
            code: meta.code().map(ToOwned::to_owned),
            message: meta.message().map(ToOwned::to_owned),
            no_response,
        }
    }
}

impl
    From<
        aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::abort_multipart_upload::AbortMultipartUploadError,
        >,
    > for Error
{
    fn from(
        error: aws_sdk_s3::error::SdkError<
            aws_sdk_s3::operation::abort_multipart_upload::AbortMultipartUploadError,
        >,
    ) -> Self {
        // keep the HTTP status, which is gone once this becomes a service error
        let status = error
            .raw_response()
            .map(|response| response.status().as_u16());
        // a timeout or a failed connection never got an answer
        let no_response = matches!(
            error,
            aws_sdk_s3::error::SdkError::TimeoutError(_)
                | aws_sdk_s3::error::SdkError::DispatchFailure(_)
        );
        // cast this error into a service error
        let service_error = error.into_service_error();
        // get this errors metadata
        let meta = service_error.meta();
        Error::S3 {
            status,
            code: meta.code().map(ToOwned::to_owned),
            message: meta.message().map(ToOwned::to_owned),
            no_response,
        }
    }
}

impl Error {
    /// Converts any S3 SDK error into an [`Error::S3`], keeping its HTTP status, code, and
    /// message, and whether any answer arrived
    ///
    /// # Arguments
    ///
    /// * `error` - The SDK error
    pub fn from_s3<E: aws_sdk_s3::error::ProvideErrorMetadata>(
        error: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>,
    ) -> Self {
        // the shared conversion keeps the status, code, and message
        s3_error(error)
    }
}

/// Converts an S3 SDK error into an [`Error::S3`], keeping its HTTP status, code, and message, and
/// whether any answer arrived
///
/// # Arguments
///
/// * `error` - The SDK error
fn s3_error<E: aws_sdk_s3::error::ProvideErrorMetadata>(
    error: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>,
) -> Error {
    use aws_sdk_s3::error::ProvideErrorMetadata;
    // keep the HTTP status of any answer
    let status = error
        .raw_response()
        .map(|response| response.status().as_u16());
    // a timeout or a failed connection never got an answer
    let no_response = matches!(
        error,
        aws_sdk_s3::error::SdkError::TimeoutError(_)
            | aws_sdk_s3::error::SdkError::DispatchFailure(_)
    );
    // a service error's code and message; other errors carry neither
    Error::S3 {
        status,
        code: error.code().map(ToOwned::to_owned),
        message: error.message().map(ToOwned::to_owned),
        no_response,
    }
}

impl From<aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>>
    for Error
{
    /// Converts a failed download into an [`Error::S3`]
    ///
    /// # Arguments
    ///
    /// * `error` - The SDK error
    fn from(
        error: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    ) -> Self {
        // the shared conversion keeps the status, code, and message
        s3_error(&error)
    }
}

impl From<aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>>
    for Error
{
    /// Converts a failed HEAD of an object into an [`Error::S3`]
    ///
    /// # Arguments
    ///
    /// * `error` - The SDK error
    fn from(
        error: aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>,
    ) -> Self {
        // the shared conversion keeps the status, code, and message
        s3_error(&error)
    }
}

impl From<aws_sdk_s3::primitives::ByteStreamError> for Error {
    fn from(error: aws_sdk_s3::primitives::ByteStreamError) -> Self {
        Error::S3ByteStream(error)
    }
}

impl From<kanal::ReceiveError> for Error {
    fn from(error: kanal::ReceiveError) -> Self {
        Error::KanalRecv(error)
    }
}

impl
    From<
        CheckArchiveError<
            OwnedPointerError<Infallible, SliceCheckError<StructCheckError>, DefaultValidatorError>,
            DefaultValidatorError,
        >,
    > for Error
{
    fn from(
        error: CheckArchiveError<
            OwnedPointerError<Infallible, SliceCheckError<StructCheckError>, DefaultValidatorError>,
            DefaultValidatorError,
        >,
    ) -> Self {
        Error::RkyvDesererialize(error.to_string())
    }
}

impl From<CheckArchiveError<StructCheckError, DefaultValidatorError>> for Error {
    fn from(error: CheckArchiveError<StructCheckError, DefaultValidatorError>) -> Self {
        Error::RkyvDesererialize(error.to_string())
    }
}

impl From<std::path::StripPrefixError> for Error {
    fn from(error: std::path::StripPrefixError) -> Self {
        Error::StripPrefix(error)
    }
}

impl From<dialoguer::Error> for Error {
    fn from(error: dialoguer::Error) -> Self {
        Error::Dialoguer(error)
    }
}
