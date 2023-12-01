pub struct ChannelInfo<Sample> {
    pub channels: Vec<Vec<Sample>>,
}

pub struct ChannelInfoRecycler {
    pub channels: usize,
    pub default_sample_count: usize,
}

impl<Sample> ChannelInfo<Sample> {
    pub fn extend_from_interleaved(&mut self, interleaved: impl ExactSizeIterator<Item = Sample>) {
        let channels = self.channels.len();
        let samples = interleaved.len() / channels;
        let hanging = interleaved.len() % channels;
        if hanging != 0 {
            tracing::warn!("Interleaved data has weird number of samples for the number of channels: {hanging} / {channels} channels will have 1 extra sample with current the current configuration");
        }

        for channel in &mut self.channels {
            channel.reserve(samples);
        }

        for (idx, sample) in interleaved.enumerate() {
            self.channels[idx % channels].push(sample);
        }
    }
}

impl<Sample> thingbuf::Recycle<ChannelInfo<Sample>> for ChannelInfoRecycler {
    fn new_element(&self) -> ChannelInfo<Sample> {
        let samples_factory = || Vec::with_capacity(self.default_sample_count);
        let channels = std::iter::repeat_with(samples_factory)
            .take(self.channels)
            .collect();

        ChannelInfo { channels }
    }

    fn recycle(&self, element: &mut ChannelInfo<Sample>) {
        for samples in &mut element.channels {
            samples.shrink_to_fit();
            samples.clear();
        }
    }
}
