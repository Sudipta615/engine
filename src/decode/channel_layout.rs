/// Semantic audio channel identifier (follows ITU-R BS.2051 / AES67).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelId {
    FrontLeft,
    FrontRight,
    Center,
    Lfe,
    SideLeft,
    SideRight,
    RearLeft,
    RearRight,
    BackCenter,
    TopFrontLeft,
    TopFrontRight,
    TopRearLeft,
    TopRearRight,
    Unknown(u8),
}

/// Semantic multi-channel layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChannelLayout {
    Mono,
    #[default]
    Stereo,
    TwoPointOne,    // FL FR LFE
    ThreePointZero, // FL FR C
    ThreePointOne,  // FL FR C LFE
    FourPointZero,  // FL FR SL SR
    FourPointOne,   // FL FR LFE SL SR
    FivePointZero,  // FL FR C SL SR
    FivePointOne,   // FL FR C LFE SL SR
    SixPointOne,    // FL FR C LFE SL SR BC
    SevenPointZero, // FL FR C SL SR RL RR
    SevenPointOne,  // FL FR C LFE SL SR RL RR
    /// 7.1.4: FL FR C LFE SL SR RL RR + four overheads (TFL TFR TRL TRR).
    SevenPointOneFour,
    Custom(Vec<ChannelId>),
}

impl ChannelLayout {
    /// Number of channels in this layout.
    pub fn channel_count(&self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::TwoPointOne => 3,
            Self::ThreePointZero => 3,
            Self::ThreePointOne => 4,
            Self::FourPointZero => 4,
            Self::FourPointOne => 5,
            Self::FivePointZero => 5,
            Self::FivePointOne => 6,
            Self::SixPointOne => 7,
            Self::SevenPointZero => 7,
            Self::SevenPointOne => 8,
            Self::SevenPointOneFour => 12,
            Self::Custom(ids) => ids.len(),
        }
    }

    /// The ordered semantic channel IDs for this layout.
    ///
    /// The order follows the conventional WAV / Symphonia channel ordering
    /// (FL, FR, C, LFE, SL, SR, RL, RR).  Downmixers, loudness weighting and
    /// channel mappers should derive channel *semantics* from this list
    /// instead of assuming `channel[2]` means "center" etc.
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        match self {
            Self::Mono => vec![ChannelId::FrontLeft],
            Self::Stereo => vec![ChannelId::FrontLeft, ChannelId::FrontRight],
            Self::TwoPointOne => vec![ChannelId::FrontLeft, ChannelId::FrontRight, ChannelId::Lfe],
            Self::ThreePointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
            ],
            Self::ThreePointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
            ],
            Self::FourPointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::FourPointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::FivePointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::FivePointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
            ],
            Self::SixPointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::BackCenter,
            ],
            Self::SevenPointZero => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::RearLeft,
                ChannelId::RearRight,
            ],
            Self::SevenPointOne => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::RearLeft,
                ChannelId::RearRight,
            ],
            Self::SevenPointOneFour => vec![
                ChannelId::FrontLeft,
                ChannelId::FrontRight,
                ChannelId::Center,
                ChannelId::Lfe,
                ChannelId::SideLeft,
                ChannelId::SideRight,
                ChannelId::RearLeft,
                ChannelId::RearRight,
                ChannelId::TopFrontLeft,
                ChannelId::TopFrontRight,
                ChannelId::TopRearLeft,
                ChannelId::TopRearRight,
            ],
            Self::Custom(ids) => ids.clone(),
        }
    }

    /// Index of the first channel with the given semantic role, if present.
    pub fn position_of(&self, id: ChannelId) -> Option<usize> {
        self.channel_ids().iter().position(|c| *c == id)
    }

    /// Build a `ChannelLayout` from a raw channel count using the
    /// conventional WAV/Symphonia channel ordering.
    pub fn from_count(n: usize) -> Self {
        match n {
            1 => Self::Mono,
            2 => Self::Stereo,
            3 => Self::ThreePointZero,
            4 => Self::FourPointZero,
            5 => Self::FivePointZero,
            6 => Self::FivePointOne,
            7 => Self::SevenPointZero,
            8 => Self::SevenPointOne,
            12 => Self::SevenPointOneFour,
            _ => Self::Custom((0..n as u8).map(ChannelId::Unknown).collect()),
        }
    }
}

// ── Explicit multichannel upmix/downmix templates ────────────────────────────
