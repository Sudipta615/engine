use std::sync::Arc;

use super::{BufferError, FixedFrameBuffer};

/// Unified handle for audio output buffers supporting both f32 and f64 precisions.
#[derive(Clone)]
pub enum OutputBuffer {
    F32(Arc<FixedFrameBuffer<f32>>),
    F64(Arc<FixedFrameBuffer<f64>>),
}

impl OutputBuffer {
    pub fn new_f32(capacity: usize) -> Result<Self, BufferError> {
        Ok(Self::F32(Arc::new(FixedFrameBuffer::<f32>::new(capacity)?)))
    }

    pub fn new_f64(capacity: usize) -> Result<Self, BufferError> {
        Ok(Self::F64(Arc::new(FixedFrameBuffer::<f64>::new(capacity)?)))
    }

    pub fn reset(&self) {
        match self {
            Self::F32(buffer) => buffer.reset(),
            Self::F64(buffer) => buffer.reset(),
        }
    }

    pub fn available(&self) -> usize {
        match self {
            Self::F32(buffer) => buffer.available(),
            Self::F64(buffer) => buffer.available(),
        }
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::F32(buffer) => buffer.capacity(),
            Self::F64(buffer) => buffer.capacity(),
        }
    }

    pub fn as_f32(&self) -> Option<&Arc<FixedFrameBuffer<f32>>> {
        match self {
            Self::F32(buffer) => Some(buffer),
            Self::F64(_) => None,
        }
    }

    pub fn as_f64(&self) -> Option<&Arc<FixedFrameBuffer<f64>>> {
        match self {
            Self::F32(_) => None,
            Self::F64(buffer) => Some(buffer),
        }
    }
}
