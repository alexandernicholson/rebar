use crate::process::ExitReason;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum RestartStrategy {
    OneForOne,
    OneForAll,
    RestForOne,
}

#[derive(Debug, Clone, Copy)]
pub enum RestartType {
    Permanent,
    Transient,
    Temporary,
}

impl RestartType {
    pub fn should_restart(&self, reason: &ExitReason) -> bool {
        match self {
            RestartType::Permanent => true,
            RestartType::Transient => !reason.is_normal(),
            RestartType::Temporary => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ShutdownStrategy {
    Timeout(Duration),
    BrutalKill,
}

pub struct SupervisorSpec {
    pub strategy: RestartStrategy,
    pub max_restarts: u32,
    pub max_seconds: u32,
    pub children: Vec<ChildSpec>,
}

impl SupervisorSpec {
    pub fn new(strategy: RestartStrategy) -> Self {
        Self {
            strategy,
            max_restarts: 3,
            max_seconds: 5,
            children: Vec::new(),
        }
    }

    pub fn max_restarts(mut self, n: u32) -> Self {
        self.max_restarts = n;
        self
    }

    pub fn max_seconds(mut self, n: u32) -> Self {
        self.max_seconds = n;
        self
    }

    pub fn child(mut self, spec: ChildSpec) -> Self {
        self.children.push(spec);
        self
    }
}

pub struct ChildSpec {
    pub id: String,
    pub restart: RestartType,
    pub shutdown: ShutdownStrategy,
}

impl ChildSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            restart: RestartType::Permanent,
            shutdown: ShutdownStrategy::Timeout(Duration::from_secs(5)),
        }
    }

    pub fn restart(mut self, restart: RestartType) -> Self {
        self.restart = restart;
        self
    }

    pub fn shutdown(mut self, strategy: ShutdownStrategy) -> Self {
        self.shutdown = strategy;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ExitReason;

    #[test]
    fn permanent_restarts_on_normal() {
        assert!(RestartType::Permanent.should_restart(&ExitReason::Normal));
    }

    #[test]
    fn permanent_restarts_on_abnormal() {
        assert!(RestartType::Permanent.should_restart(&ExitReason::Abnormal("err".into())));
    }

    #[test]
    fn permanent_restarts_on_kill() {
        assert!(RestartType::Permanent.should_restart(&ExitReason::Kill));
    }

    #[test]
    fn transient_does_not_restart_on_normal() {
        assert!(!RestartType::Transient.should_restart(&ExitReason::Normal));
    }

    #[test]
    fn transient_restarts_on_abnormal() {
        assert!(RestartType::Transient.should_restart(&ExitReason::Abnormal("err".into())));
    }

    #[test]
    fn temporary_never_restarts() {
        assert!(!RestartType::Temporary.should_restart(&ExitReason::Normal));
        assert!(!RestartType::Temporary.should_restart(&ExitReason::Abnormal("err".into())));
        assert!(!RestartType::Temporary.should_restart(&ExitReason::Kill));
    }

    #[test]
    fn supervisor_spec_builder() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForOne)
            .max_restarts(3)
            .max_seconds(5);
        assert_eq!(spec.max_restarts, 3);
        assert_eq!(spec.max_seconds, 5);
    }

    #[test]
    fn supervisor_spec_with_children() {
        let spec = SupervisorSpec::new(RestartStrategy::OneForAll)
            .child(ChildSpec::new("worker_1"))
            .child(ChildSpec::new("worker_2"));
        assert_eq!(spec.children.len(), 2);
    }

    #[test]
    fn child_spec_defaults() {
        let child = ChildSpec::new("test");
        assert_eq!(child.id, "test");
        assert!(matches!(child.restart, RestartType::Permanent));
        assert!(matches!(child.shutdown, ShutdownStrategy::Timeout(_)));
    }

    #[test]
    fn child_spec_builder() {
        let child = ChildSpec::new("worker")
            .restart(RestartType::Transient)
            .shutdown(ShutdownStrategy::BrutalKill);
        assert!(matches!(child.restart, RestartType::Transient));
        assert!(matches!(child.shutdown, ShutdownStrategy::BrutalKill));
    }
}
