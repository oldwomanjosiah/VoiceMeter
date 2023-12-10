use std::{fmt::Write, sync::Arc};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Hz(isize);

pub trait HzExt: Sized {
    fn hz(self) -> Hz;

    #[inline(always)]
    fn khz(self) -> Hz {
        self.hz() * 1000
    }

    #[inline(always)]
    fn mhz(self) -> Hz {
        self.khz() * 1000
    }

    #[inline(always)]
    fn ghz(self) -> Hz {
        self.mhz() * 1000
    }
}

macro_rules! hz_ext_cast {
    ($($ty:ty),+) => {
        $(
            impl HzExt for $ty {
                #[inline(always)]
                fn hz(self) -> Hz {
                    Hz(self as _)
                }
            }

            impl std::ops::Mul<$ty> for Hz {
                type Output = Hz;

                #[inline]
                fn mul(self, rhs: $ty) -> Hz {
                    (self.0 * rhs as isize).hz()
                }
            }

            impl std::ops::Div<$ty> for Hz {
                type Output = Hz;

                #[inline]
                fn div(self, rhs: $ty) -> Hz {
                    (self.0 / rhs as isize).hz()
                }
            }
        )*
    };
}

impl std::ops::Add for Hz {
    type Output = Hz;

    #[inline]
    fn add(self, rhs: Hz) -> Self::Output {
        (self.0 + rhs.0).hz()
    }
}

impl std::ops::Sub for Hz {
    type Output = Hz;

    #[inline]
    fn sub(self, rhs: Hz) -> Self::Output {
        (self.0 - rhs.0).hz()
    }
}

impl std::ops::Div for Hz {
    type Output = f32;

    #[inline]
    fn div(self, rhs: Self) -> Self::Output {
        self.0 as f32 / rhs.0 as f32
    }
}

hz_ext_cast![u8, i8, u16, i16, u32, i32, u64, i64, usize, isize];

impl std::fmt::Display for Hz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        static LEADING: &'static str = "kmgp";

        let neg = self.0 < 0;
        let mut val = self.0.abs();
        let mut leading = LEADING;

        while val % 1000 == 0 && leading.len() > 1 {
            val %= 1000;
            leading = &leading[1..];
        }

        if neg {
            f.write_char('-')?;
        }

        let char = &leading[..1];
        let div = val / 1000;
        let rem = val % 1000;

        if rem == 0 {
            f.write_fmt(format_args!("{div} {char}Hz"))
        } else {
            f.write_fmt(format_args!("{div}.{rem:04} {char}Hz"))
        }
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
