//! Audio bus and sidechain routing abstraction for plugins (Item 28).

use crate::MAX_PLUGIN_CHANNELS;

/// Maximum number of audio buses a plugin may expose (e.g. main in/out, sidechain, aux).
pub const MAX_PLUGIN_BUSSES: usize = 4;

/// Role of an audio bus.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginBusRole {
    MainInput = 0,
    MainOutput = 1,
    SidechainInput = 2,
    AuxiliaryOutput = 3,
}

/// Description of a single audio bus.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusDescriptor {
    pub role: PluginBusRole,
    pub channels: u32,
}

impl BusDescriptor {
    pub const fn main_stereo(role: PluginBusRole) -> Self {
        Self { role, channels: 2 }
    }

    pub const fn sidechain_stereo() -> Self {
        Self {
            role: PluginBusRole::SidechainInput,
            channels: 2,
        }
    }
}

/// A multi-bus audio block configuration passed to the plugin.
#[derive(Debug, Clone)]
pub struct PluginBusLayout {
    pub busses: [BusDescriptor; MAX_PLUGIN_BUSSES],
    pub bus_count: usize,
}

impl Default for PluginBusLayout {
    fn default() -> Self {
        Self::stereo_with_optional_sidechain(false)
    }
}

impl PluginBusLayout {
    pub fn stereo_with_optional_sidechain(sidechain: bool) -> Self {
        let mut busses = [BusDescriptor {
            role: PluginBusRole::MainInput,
            channels: 2,
        }; MAX_PLUGIN_BUSSES];
        busses[0] = BusDescriptor {
            role: PluginBusRole::MainInput,
            channels: 2,
        };
        busses[1] = BusDescriptor {
            role: PluginBusRole::MainOutput,
            channels: 2,
        };
        let mut count = 2;
        if sidechain {
            busses[2] = BusDescriptor {
                role: PluginBusRole::SidechainInput,
                channels: 2,
            };
            count = 3;
        }
        Self {
            busses,
            bus_count: count,
        }
    }
}

/// Safe facade for multi-bus audio block access during processing.
pub struct AudioBussesMut<'a> {
    /// Main audio channel planes (in-place processing).
    pub main: &'a mut [&'a mut [f32]],
    /// Optional sidechain input channel planes (read-only).
    pub sidechain: Option<&'a [&'a [f32]]>,
}

impl<'a> AudioBussesMut<'a> {
    pub fn new(main: &'a mut [&'a mut [f32]]) -> Self {
        Self {
            main,
            sidechain: None,
        }
    }

    pub fn with_sidechain(main: &'a mut [&'a mut [f32]], sidechain: &'a [&'a [f32]]) -> Self {
        Self {
            main,
            sidechain: Some(sidechain),
        }
    }

    pub fn frames(&self) -> usize {
        self.main.first().map(|ch| ch.len()).unwrap_or(0)
    }

    pub fn main_channels(&self) -> usize {
        self.main.len().min(MAX_PLUGIN_CHANNELS)
    }

    pub fn sidechain_channels(&self) -> usize {
        self.sidechain.map(|sc| sc.len()).unwrap_or(0)
    }
}
