//! Sample-accurate parameter automation for plugins (Item 28).

/// Maximum parameter automation events per audio block.
pub const MAX_AUTOMATION_EVENTS_PER_BLOCK: usize = 64;

/// A sample-offset parameter change event within an audio block.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamAutomationEvent {
    /// Sample offset within the audio block [0..frames).
    pub frame_offset: u32,
    /// Parameter index on the plugin.
    pub param_index: u32,
    /// New parameter value.
    pub value: f32,
}

/// A batch of parameter automation events crossing the ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ParamAutomationBatch {
    pub events: [ParamAutomationEvent; MAX_AUTOMATION_EVENTS_PER_BLOCK],
    pub count: u32,
}

impl Default for ParamAutomationBatch {
    fn default() -> Self {
        Self::empty()
    }
}

impl ParamAutomationBatch {
    pub const fn empty() -> Self {
        Self {
            events: [ParamAutomationEvent {
                frame_offset: 0,
                param_index: 0,
                value: 0.0,
            }; MAX_AUTOMATION_EVENTS_PER_BLOCK],
            count: 0,
        }
    }

    pub fn push(&mut self, event: ParamAutomationEvent) -> bool {
        if (self.count as usize) < MAX_AUTOMATION_EVENTS_PER_BLOCK {
            self.events[self.count as usize] = event;
            self.count += 1;
            true
        } else {
            false
        }
    }

    pub fn as_slice(&self) -> &[ParamAutomationEvent] {
        &self.events[..self.count as usize]
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }
}
