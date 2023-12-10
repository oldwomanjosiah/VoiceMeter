use std::{fmt::Write, sync::Arc};

use connection::managed::{LoadError, ManagedConnection};
use tracing::Span;
use ui::Loadable;

pub mod connection;
pub mod ui;

pub mod fft {
    use std::{
        os::windows,
        sync::{Arc, Mutex},
    };

    use rustfft::{
        num_complex::Complex,
        num_traits::{AsPrimitive, FromPrimitive},
        FftDirection, FftNum, FftPlanner,
    };

    use crate::Hz;

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum FftSize {
        Samples1024 = 1024,
        #[default]
        Samples2048 = 2048,
        Samples4096 = 4096,
    }

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum PreprocessWindow {
        #[default]
        None,
    }

    #[derive(Clone)]
    pub struct Builder<T: FftNum> {
        planner: Arc<Mutex<FftPlanner<T>>>,
        size: FftSize,
        direction: FftDirection,
        window: PreprocessWindow,
    }

    pub struct Fft<T: FftNum> {
        delegate: Arc<dyn rustfft::Fft<T>>,
        has_result: bool,
        buffer: Vec<Complex<T>>,
        scratch: Vec<Complex<T>>,
        window: PreprocessWindow,
    }

    pub struct FftResult<T: FftNum> {
        pub value: Complex<T>,
        pub index: usize,
        pub max: usize,
    }

    impl<T: FftNum + rustfft::num_traits::Float> FftResult<T> {
        #[inline]
        pub fn frequency(&self, sample_rate: impl Into<Hz>) -> Hz {
            sample_rate.into() * self.index / self.max / 2
        }

        #[inline]
        pub fn amplitude(&self) -> T {
            self.value.norm()
        }
    }

    impl<T: FftNum> Builder<T> {
        pub fn with_size(self, size: impl Into<FftSize>) -> Self {
            Self {
                size: size.into(),
                ..self
            }
        }

        pub fn with_direction(self, direction: impl Into<FftDirection>) -> Self {
            Self {
                direction: direction.into(),
                ..self
            }
        }

        pub fn with_window(self, window: PreprocessWindow) -> Self {
            Self { window, ..self }
        }

        pub fn build(&self) -> Fft<T> {
            let size = self.size as _;
            let delegate = {
                let mut planner = self.planner.lock().unwrap();
                planner.plan_fft(size, self.direction)
            };
            let scratch_len = delegate.get_inplace_scratch_len();

            let zero: Complex<T> = Complex {
                re: T::zero(),
                im: T::zero(),
            };

            let buffer = vec![zero; size];
            let scratch = vec![zero; scratch_len];
            let window = self.window;

            Fft {
                delegate,
                has_result: false,
                window,
                buffer,
                scratch,
            }
        }
    }

    impl<T: FftNum> Fft<T> {
        pub fn builder() -> Builder<T> {
            Builder {
                planner: Arc::new(Mutex::new(FftPlanner::new())),
                size: Default::default(),
                window: Default::default(),
                direction: FftDirection::Forward,
            }
        }

        pub fn set_window(&mut self, window: PreprocessWindow) {
            self.window = window;
        }

        pub fn window(&self) -> PreprocessWindow {
            self.window
        }

        pub fn size(&self) -> FftSize {
            FftSize::from(self.buffer.len())
        }

        pub fn output_length(&self) -> usize {
            self.buffer.len() / 2
        }
    }

    impl<T: FftNum> Fft<T>
    where
        usize: rustfft::num_traits::AsPrimitive<T>,
        T: rustfft::num_traits::Float,
    {
        pub fn get_result(&self) -> Option<impl ExactSizeIterator<Item = FftResult<T>> + '_> {
            if !self.has_result {
                return None;
            }

            let max = self.output_length();

            let out = self
                .buffer
                .iter()
                .take(max)
                .enumerate()
                .map(move |(index, &value)| FftResult {
                    value: value / max.as_().sqrt(),
                    index,
                    max,
                });

            Some(out)
        }

        pub fn process_fft_converting<I>(
            &mut self,
            data: impl ExactSizeIterator<Item = I>,
            convert: impl Fn(I) -> T,
        ) -> Option<impl ExactSizeIterator<Item = FftResult<T>> + '_> {
            if data.len() < self.buffer.len() {
                tracing::warn!("Cannot process fft with fewer samples than length");
                return None;
            }

            let size = self.buffer.len();
            let skip = data.len().saturating_sub(size);
            let data = data.skip(skip);
            let zero = T::zero();

            for (idx, (slot, data)) in self.buffer.iter_mut().zip(data).enumerate() {
                let intermediate = convert(data);
                let windowed = self.window.process(intermediate, idx, size);
                *slot = Complex::new(windowed, zero);
            }

            self.delegate
                .process_with_scratch(&mut self.buffer, &mut self.scratch);

            self.has_result = true;
            self.get_result()
        }

        pub fn process_samples<I>(
            &mut self,
            data: impl ExactSizeIterator<Item = I>,
        ) -> Option<impl ExactSizeIterator<Item = FftResult<T>> + '_>
        where
            I: rustfft::num_traits::AsPrimitive<T>,
            I: rustfft::num_traits::Bounded,
        {
            let max: T = I::max_value().as_();
            self.process_fft_converting(data, move |intermediate| {
                let intermediate: T = intermediate.as_();
                intermediate / max
            })
        }
    }

    impl From<usize> for FftSize {
        fn from(value: usize) -> Self {
            match value {
                ..=1024 => FftSize::Samples1024,
                ..=2048 => FftSize::Samples2048,
                _ => FftSize::Samples4096,
            }
        }
    }

    impl PreprocessWindow {
        pub fn process<T: FftNum>(self, input: T, idx: usize, count: usize) -> T {
            match self {
                PreprocessWindow::None => input,
            }
        }

        // TODO(josiah) add in portable simd implementation, based on nightly feature added to crate
    }
}

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
