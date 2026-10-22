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
    /// Held across a whole add, and a removal up to its sinks being
    /// detached. They are attached and detached by name, so one being
    /// removed must be off every listener before another of its name is
    /// attached; waiting for its source is left outside, so a camera that
    /// takes its timeout to let go holds up no other add or remove.
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

    /// Makes a channel and starts it.
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
        let removed = {
            let _changing = self.changing.lock().unwrap_or_else(|p| p.into_inner());
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
                channel.stop();
                true
            }
            None => false,
        }
    }

    /// Removes and stops every channel, all at once: each is halted first,
    /// and then each waited for on a thread of its own, so the whole takes
    /// a camera's timeout at most rather than one for every camera.
    pub(crate) fn clear(&self) {
        let all = {
            let _changing = self.changing.lock().unwrap_or_else(|p| p.into_inner());
            let all = std::mem::take(&mut *self.map.write().unwrap_or_else(|p| p.into_inner()));
            for channel in all.values() {
                channel.halt();
            }
            all
        };
        std::thread::scope(|scope| {
            for channel in all.values() {
                scope.spawn(|| channel.stop());
            }
        });
    }
}
