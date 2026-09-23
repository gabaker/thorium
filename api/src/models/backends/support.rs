//! Traits allowing for backends to advertize support for various functionality in Thorium

mod grep;
mod notifications;
mod outputs;
mod tags;

#[cfg(feature = "api")]
mod graphics;

pub use grep::GrepSupport;
pub use notifications::NotificationSupport;
pub use outputs::OutputSupport;
pub use tags::TagSupport;

#[cfg(feature = "api")]
pub(crate) use graphics::GraphicSupport;
