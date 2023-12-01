use std::sync::Arc;

use connection::managed::{LoadError, ManagedConnection};
use tracing::Span;
use ui::Loadable;

pub mod connection;
pub mod ui;

#[derive(Debug, Clone)]
pub struct SharedSpan(Option<Arc<Span>>);

#[must_use = "A Span Guard must be held until you wish to exit the span"]
pub struct SharedSpanGuard<'s>(Option<tracing::span::Entered<'s>>);

impl From<Span> for SharedSpan {
    fn from(value: Span) -> Self {
        SharedSpan(Some(Arc::new(value)))
    }
}

impl From<Option<Span>> for SharedSpan {
    fn from(value: Option<Span>) -> Self {
        SharedSpan(value.map(Arc::new))
    }
}

impl SharedSpan {
    pub fn enter(&self) -> SharedSpanGuard<'_> {
        SharedSpanGuard(self.0.as_ref().map(|it| it.enter()))
    }
}

impl<S> Loadable<Vec<ManagedConnection<S>>, LoadError>
    for crate::connection::managed::LoadingDevices<S>
{
    fn ready(&mut self) -> bool {
        crate::connection::managed::LoadingDevices::ready(self)
    }

    fn take(&mut self) -> Result<Vec<ManagedConnection<S>>, LoadError> {
        crate::connection::managed::LoadingDevices::take(self)
    }
}
