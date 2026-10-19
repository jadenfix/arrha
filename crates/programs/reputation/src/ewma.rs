use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ewma {
    alpha: f64,
    value: f64,
    initialized: bool,
}

impl Ewma {
    pub fn new(alpha: f64) -> Self {
        Ewma {
            alpha,
            value: 0.0,
            initialized: false,
        }
    }

    pub fn update(&mut self, sample: f64) {
        if !self.initialized {
            self.value = sample;
            self.initialized = true;
        } else {
            self.value = self.alpha * self.value + (1.0 - self.alpha) * sample;
        }
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn initialized(&self) -> bool {
        self.initialized
    }
}
