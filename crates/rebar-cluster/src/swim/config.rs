use std::time::Duration;

/// Configuration for the SWIM protocol.
#[derive(Debug, Clone)]
pub struct SwimConfig {
    /// How often the protocol runs a probe cycle.
    pub protocol_period: Duration,
    /// How long a node stays in `Suspect` state before being declared dead.
    pub suspect_timeout: Duration,
    /// How long a dead node is kept in the membership list before removal.
    pub dead_removal_delay: Duration,
    /// Number of indirect probes to send when a direct probe fails.
    pub indirect_probe_count: usize,
    /// Maximum gossip messages piggy-backed per protocol tick.
    pub max_gossip_per_tick: usize,
}

impl Default for SwimConfig {
    fn default() -> Self {
        Self {
            protocol_period: Duration::from_secs(1),
            suspect_timeout: Duration::from_secs(5),
            dead_removal_delay: Duration::from_secs(30),
            indirect_probe_count: 3,
            max_gossip_per_tick: 8,
        }
    }
}

impl SwimConfig {
    /// Returns a new builder with default values.
    pub fn builder() -> SwimConfigBuilder {
        SwimConfigBuilder::default()
    }
}

/// Builder for `SwimConfig`.
#[derive(Debug, Clone)]
pub struct SwimConfigBuilder {
    config: SwimConfig,
}

impl Default for SwimConfigBuilder {
    fn default() -> Self {
        Self {
            config: SwimConfig::default(),
        }
    }
}

impl SwimConfigBuilder {
    pub fn protocol_period(mut self, d: Duration) -> Self {
        self.config.protocol_period = d;
        self
    }

    pub fn suspect_timeout(mut self, d: Duration) -> Self {
        self.config.suspect_timeout = d;
        self
    }

    pub fn dead_removal_delay(mut self, d: Duration) -> Self {
        self.config.dead_removal_delay = d;
        self
    }

    pub fn indirect_probe_count(mut self, n: usize) -> Self {
        self.config.indirect_probe_count = n;
        self
    }

    pub fn max_gossip_per_tick(mut self, n: usize) -> Self {
        self.config.max_gossip_per_tick = n;
        self
    }

    pub fn build(self) -> SwimConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let config = SwimConfig::default();
        assert_eq!(config.protocol_period, Duration::from_secs(1));
        assert_eq!(config.suspect_timeout, Duration::from_secs(5));
        assert_eq!(config.dead_removal_delay, Duration::from_secs(30));
        assert_eq!(config.indirect_probe_count, 3);
        assert_eq!(config.max_gossip_per_tick, 8);
    }

    #[test]
    fn config_builder_overrides() {
        let config = SwimConfig::builder()
            .protocol_period(Duration::from_millis(500))
            .suspect_timeout(Duration::from_secs(10))
            .dead_removal_delay(Duration::from_secs(60))
            .indirect_probe_count(5)
            .max_gossip_per_tick(16)
            .build();

        assert_eq!(config.protocol_period, Duration::from_millis(500));
        assert_eq!(config.suspect_timeout, Duration::from_secs(10));
        assert_eq!(config.dead_removal_delay, Duration::from_secs(60));
        assert_eq!(config.indirect_probe_count, 5);
        assert_eq!(config.max_gossip_per_tick, 16);
    }
}
