//! Every channel the server has, by name: made and removed over the API,
//! none kept across a restart — whatever manages the inputs makes them
//! again.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::channel::{Channel, ChannelConfig, ChannelError, Listeners};

pub(crate) struct Channels {
    /// Read on every request, written only as a channel is added or
    /// removed: a reader takes the channel it needs out and lets go of the
    /// lock before doing anything that waits.
    map: RwLock<BTreeMap<String, Arc<Channel>>>,
    /// Held across a whole add or remove. A channel's sinks are attached
    /// and detached by name, so one being removed must be off every
    /// listener before another of its name is attached — and the removal
    /// waits on its source, which the map's lock must not.
    changing: Mutex<()>,
    listeners: Listeners,
}

impl Channels {
    pub(crate) fn new(listeners: Listeners) -> Self {
        Self {
            map: RwLock::default(),
            changing: Mutex::default(),
            listeners,
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

    /// Makes a channel and starts it. May wait for a removal under way —
    /// up to a camera's timeout.
    pub(crate) fn add(&self, config: &ChannelConfig) -> Result<Arc<Channel>, ChannelError> {
        let _changing = self.changing.lock().unwrap_or_else(|p| p.into_inner());
        if self.get(&config.name).is_some() {
            return Err(ChannelError::Exists(config.name.clone()));
        }
        let channel = Arc::new(Channel::start(config, &self.listeners)?);
        self.map
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(config.name.clone(), Arc::clone(&channel));
        Ok(channel)
    }

    /// Removes a channel and stops it, returning once it is off every
    /// listener and its source has stopped — up to a camera's timeout.
    /// Whether there was one.
    pub(crate) fn remove(&self, name: &str) -> bool {
        let _changing = self.changing.lock().unwrap_or_else(|p| p.into_inner());
        let removed = self
            .map
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(name);
        match removed {
            Some(channel) => {
                channel.stop();
                true
            }
            None => false,
        }
    }

    /// Removes and stops every channel.
    pub(crate) fn clear(&self) {
        let _changing = self.changing.lock().unwrap_or_else(|p| p.into_inner());
        let all = std::mem::take(&mut *self.map.write().unwrap_or_else(|p| p.into_inner()));
        for channel in all.values() {
            channel.stop();
        }
    }
}
