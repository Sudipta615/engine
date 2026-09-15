//! Unified Modulation Matrix (Item 31).

use serde::{Deserialize, Serialize};

/// Modulation source identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModSource {
    Lfo(usize),
    Envelope(usize),
    Follower(usize),
    MidiCc(u8),
    Constant(u32),
}

/// A routing connection from a modulation source to a destination parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModRoute {
    pub source: ModSource,
    pub target_param: u32,
    pub depth: f32,
    pub bipolar: bool,
    pub enabled: bool,
}

impl ModRoute {
    pub fn new(source: ModSource, target_param: u32, depth: f32) -> Self {
        Self {
            source,
            target_param,
            depth,
            bipolar: true,
            enabled: true,
        }
    }
}

/// Unified modulation matrix connecting modulation sources to targets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModulationMatrix {
    pub routes: Vec<ModRoute>,
}

impl ModulationMatrix {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Add a modulation connection.
    pub fn connect(&mut self, source: ModSource, target_param: u32, depth: f32) {
        self.routes.push(ModRoute::new(source, target_param, depth));
    }

    /// Clear all routes.
    pub fn clear(&mut self) {
        self.routes.clear();
    }

    /// Compute cumulative modulation offset for a specific target parameter.
    /// Fast inline accumulation, allocation-free.
    pub fn evaluate_param_offset<F>(&self, target_param: u32, mut source_value_lookup: F) -> f32
    where
        F: FnMut(ModSource) -> f32,
    {
        let mut sum = 0.0f32;
        for route in &self.routes {
            if route.enabled && route.target_param == target_param {
                let raw_val = source_value_lookup(route.source);
                let val = if route.bipolar {
                    raw_val
                } else {
                    raw_val.max(0.0)
                };
                sum += val * route.depth;
            }
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modulation_matrix_routing() {
        let mut matrix = ModulationMatrix::new();
        matrix.connect(ModSource::Lfo(0), 10, 0.5);
        matrix.connect(ModSource::Envelope(0), 10, 0.2);

        let offset = matrix.evaluate_param_offset(10, |src| match src {
            ModSource::Lfo(0) => 1.0,
            ModSource::Envelope(0) => 0.5,
            _ => 0.0,
        });

        // 1.0 * 0.5 + 0.5 * 0.2 = 0.5 + 0.1 = 0.6
        assert!((offset - 0.6).abs() < 1e-4);
    }
}
