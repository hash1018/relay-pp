//! Every channel the server has, by name: made and removed over the API,
//! none kept across a restart — whatever manages the inputs makes them
//! again.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::runtime::Handle;

use crate::channel::{AttachError, Channel, ChannelConfig, ChannelError, Listeners, Protocol};
use crate::config::{ConvertConfig, RecordConfig};

pub(crate) struct Channels {
    /// Read on every request, written only as a channel is added or
    /// removed: a reader takes the channel it needs out and lets go of the
    /// lock before doing anything that waits.
    map: RwLock<BTreeMap<String, Arc<Channel>>>,
    /// Held across a whole add, a registration on a listener, and a
    /// removal up to its registrations being taken off. They are made and
    /// taken off by name, so a channel removed must be off every listener
    /// before another of its name is registered; waiting for what it ends
    /// is left outside, so a camera slow to let go holds nothing up.
    changing: Mutex<()>,
    listeners: Listeners,
    /// How a recorded channel is recorded.
    pub(crate) record: RecordConfig,
    /// What a channel converts, where it says nothing of it.
    convert: ConvertConfig,
    /// Where every channel's tasks run.
    runtime: Handle,
}

impl Channels {
    pub(crate) fn new(
        listeners: Listeners,
        record: RecordConfig,
        convert: ConvertConfig,
        runtime: Handle,
    ) -> Self {
        Self {
            map: RwLock::default(),
            changing: Mutex::default(),
            listeners,
            record,
            convert,
            runtime,
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<Arc<Channel>> {
        self.map
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
    }

    /// Every channel, by name.
    pub(crate) fn list(&self) -> Vec<Arc<Channel>> {
        self.map
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Makes a channel of its source alone and starts pulling it.
    pub(crate) fn add(&self, config: &ChannelConfig) -> Result<Arc<Channel>, ChannelError> {
        let _changing = self.changing();
        if self.get(&config.name).is_some() {
            return Err(ChannelError::Exists(config.name.clone()));
        }
        let channel = Arc::new(Channel::start(
            config,
            &self.record,
            self.convert,
            &self.runtime,
        )?);
        self.map
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(config.name.clone(), Arc::clone(&channel));
        Ok(channel)
    }

    /// The channel called `name`, registered on `protocol`'s listener
    /// where it was not yet.
    pub(crate) fn stream(
        &self,
        name: &str,
        protocol: Protocol,
    ) -> Result<Arc<Channel>, AttachError> {
        let _changing = self.changing();
        let channel = self
            .get(name)
            .ok_or_else(|| AttachError::NoChannel(name.to_owned()))?;
        channel.stream(protocol, &self.listeners)?;
        Ok(channel)
    }

    /// Has `channel` registered on the WebRTC listener reading what WebRTC
    /// players read of it — see [`Channel::webrtc`].
    pub(crate) async fn webrtc(&self, channel: &Channel, within: Duration) -> Result<(), String> {
        channel.webrtc(&self.listeners, within).await
    }

    /// Begins recording the channel called `name`.
    pub(crate) fn record(&self, name: &str) -> Result<(), AttachError> {
        self.get(name)
            .ok_or_else(|| AttachError::NoChannel(name.to_owned()))?
            .attach_record()
    }

    /// Ends the recording of the channel called `name`, returning once it
    /// is finished, its playlist ended.
    pub(crate) async fn unrecord(&self, name: &str) -> Result<(), AttachError> {
        let recorder = self
            .get(name)
            .ok_or_else(|| AttachError::NoChannel(name.to_owned()))?
            .detach_record()?;
        recorder.wait().await;
        Ok(())
    }

    /// Removes a channel and stops it, returning once it is off every
    /// listener, its recording finished and its source's task ended.
    /// Whether there was one.
    pub(crate) async fn remove(&self, name: &str) -> bool {
        let removed = {
            let _changing = self.changing();
            let removed = self
                .map
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .remove(name);
            if let Some(channel) = &removed {
                channel.halt();
            }
            removed
        };
        match removed {
            Some(channel) => {
                channel.stop().await;
                true
            }
            None => false,
        }
    }

    /// Removes and stops every channel, all at once: each is halted first,
    /// so their tasks end together, and then each waited for.
    pub(crate) async fn clear(&self) {
        let all = {
            let _changing = self.changing();
            let all = std::mem::take(&mut *self.map.write().unwrap_or_else(|p| p.into_inner()));
            for channel in all.values() {
                channel.halt();
            }
            all
        };
        for channel in all.values() {
            channel.stop().await;
        }
    }

    fn changing(&self) -> std::sync::MutexGuard<'_, ()> {
        self.changing.lock().unwrap_or_else(|p| p.into_inner())
    }
}
