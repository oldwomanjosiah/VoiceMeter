use std::time::Duration;

use cpal::{BuildStreamError, FromSample, StreamError};
use eyre::Result;

pub mod buffer;
pub mod channels;
pub mod managed;

macro_rules! def_all_convert {
    ($($ty:ty),+ $(,)?) => {
        /// Sample types which may be converted to any other sample type.
        pub trait AllConvertable:
            $(::cpal::FromSample<$ty> +)* std::any::Any {}

        $(impl AllConvertable for $ty {})*
    };
}

def_all_convert![f32, f64, i8, i16, i32, i64, u8, u16, u32, u64];

type SampleIter<'l, S> = dyn ExactSizeIterator<Item = S> + 'l;

/// Build an input stream, converting it's native sample type to the caller's preferred sample type for their callbacks.
fn build_input_stream_converting<Device, Sample>(
    device: &Device,
    config: &cpal::StreamConfig,
    format: cpal::SampleFormat,
    data_callback: impl FnMut(&mut SampleIter<Sample>, &cpal::InputCallbackInfo) + Send + 'static,
    error_callback: impl FnMut(StreamError) + Send + 'static,
    timeout: Option<Duration>,
) -> Result<Device::Stream, BuildStreamError>
where
    Device: cpal::traits::DeviceTrait,
    Sample: AllConvertable,
{
    fn build_callback<InType: Copy, Sample: FromSample<InType>>(
        mut data_callback: impl FnMut(&mut SampleIter<'_, Sample>, &cpal::InputCallbackInfo),
    ) -> impl FnMut(&[InType], &cpal::InputCallbackInfo) {
        tracing::info!(
            "Wrapping callback for sample type {} in converter from type {}",
            std::any::type_name::<Sample>(),
            std::any::type_name::<InType>()
        );

        struct Iterator<'d, I, O> {
            inner: &'d [I],
            _pd: std::marker::PhantomData<O>,
        }

        impl<I: Copy, O: FromSample<I>> std::iter::Iterator for Iterator<'_, I, O> {
            type Item = O;

            fn next(&mut self) -> Option<Self::Item> {
                match self.inner {
                    [next, rest @ ..] => {
                        self.inner = rest;
                        Some(FromSample::from_sample_(*next))
                    }

                    [] => None,
                }
            }
        }

        impl<I: Copy, O: FromSample<I>> std::iter::ExactSizeIterator for Iterator<'_, I, O> {
            fn len(&self) -> usize {
                self.inner.len()
            }
        }

        move |samples, info| {
            let mut iterator = Iterator {
                inner: samples,
                _pd: Default::default(),
            };
            data_callback(&mut iterator, info)
        }
    }

    /// Build the callbacks for converting between types
    macro_rules! format_switch {
        (build_input_stream($format:ident, $device:ident, $config:ident, $data_builder:expr, $error_callback:ident, $timeout:ident) forall { $($f:ident => $ty:ty),+ $(,)? } fallback |$cap:ident| $with:expr) => {
            match $format {
                $(
                    ::cpal::SampleFormat::$f => ::cpal::traits::DeviceTrait::build_input_stream::<$ty, _, _>(
                        $device,
                        $config,
                        $data_builder,
                        $error_callback,
                        $timeout
                    ),
                )*

                $cap => $with
            }
        };
    }

    format_switch!(
        build_input_stream(
            format,
            device,
            config,
            build_callback(data_callback),
            error_callback,
            timeout
        ) forall {
            I8 => i8,
            I16 => i16,
            I32 => i32,
            I64 => i64,
            U8 => u8,
            U16 => u16,
            U32 => u32,
            U64 => u64,
            F32 => f32,
            F64 => f64,
        } fallback |it| {
            tracing::warn!("Stream format {it:?} is not known");
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        }
    )
}
