//! MIDI and MPE event handling for plugins (Item 28).

/// Maximum number of MIDI events per audio block.
pub const MAX_MIDI_EVENTS_PER_BLOCK: usize = 128;

/// MIDI event types.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiEventType {
    NoteOff = 0x80,
    NoteOn = 0x90,
    PolyphonicPressure = 0xA0,
    ControlChange = 0xB0,
    ProgramChange = 0xC0,
    ChannelPressure = 0xD0,
    PitchBend = 0xE0,
    MpePressure = 0xF0,
    MpeTimbre = 0xF1,
}

/// A sample-accurate MIDI event within an audio block.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiEvent {
    /// Sample offset within the audio block [0..frames).
    pub frame_offset: u32,
    /// MIDI status byte / event type.
    pub event_type: u8,
    /// MIDI channel [0..15].
    pub channel: u8,
    /// First data byte (note number, CC number, etc.).
    pub data1: u8,
    /// Second data byte (velocity, CC value, etc.).
    pub data2: u8,
    /// Normalized continuous value [-1.0..1.0] for high-res pitch bend / MPE.
    pub normalized_value: f32,
}

impl MidiEvent {
    pub const fn note_on(frame_offset: u32, channel: u8, note: u8, velocity: u8) -> Self {
        Self {
            frame_offset,
            event_type: MidiEventType::NoteOn as u8,
            channel,
            data1: note,
            data2: velocity,
            normalized_value: (velocity as f32) / 127.0,
        }
    }

    pub const fn note_off(frame_offset: u32, channel: u8, note: u8) -> Self {
        Self {
            frame_offset,
            event_type: MidiEventType::NoteOff as u8,
            channel,
            data1: note,
            data2: 0,
            normalized_value: 0.0,
        }
    }

    pub const fn control_change(frame_offset: u32, channel: u8, cc: u8, value: u8) -> Self {
        Self {
            frame_offset,
            event_type: MidiEventType::ControlChange as u8,
            channel,
            data1: cc,
            data2: value,
            normalized_value: (value as f32) / 127.0,
        }
    }

    pub fn pitch_bend(frame_offset: u32, channel: u8, semitones: f32) -> Self {
        Self {
            frame_offset,
            event_type: MidiEventType::PitchBend as u8,
            channel,
            data1: 0,
            data2: 0,
            normalized_value: semitones.clamp(-1.0, 1.0),
        }
    }
}

/// Fixed-capacity MIDI event buffer crossing the ABI boundary without heap allocations.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MidiPacket {
    pub events: [MidiEvent; MAX_MIDI_EVENTS_PER_BLOCK],
    pub count: u32,
}

impl Default for MidiPacket {
    fn default() -> Self {
        Self::empty()
    }
}

impl MidiPacket {
    pub const fn empty() -> Self {
        Self {
            events: [MidiEvent {
                frame_offset: 0,
                event_type: 0,
                channel: 0,
                data1: 0,
                data2: 0,
                normalized_value: 0.0,
            }; MAX_MIDI_EVENTS_PER_BLOCK],
            count: 0,
        }
    }

    pub fn push(&mut self, event: MidiEvent) -> bool {
        if (self.count as usize) < MAX_MIDI_EVENTS_PER_BLOCK {
            self.events[self.count as usize] = event;
            self.count += 1;
            true
        } else {
            false
        }
    }

    pub fn as_slice(&self) -> &[MidiEvent] {
        &self.events[..self.count as usize]
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }
}
