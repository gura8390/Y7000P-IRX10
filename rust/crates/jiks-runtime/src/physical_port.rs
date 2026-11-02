//! Native ticket boundary; test ports cannot reach WMI or synthesize input.
use anyhow::Result;
use jiks_platform::{Completion, ExplicitAction, HardwareAction, PlatformWorker, Ticket};
use std::time::Duration;
pub(crate) trait PhysicalTicket: Send {
    fn wait(&mut self, timeout: Duration) -> Result<Completion>;
}
impl PhysicalTicket for Ticket {
    fn wait(&mut self, timeout: Duration) -> Result<Completion> {
        Ticket::wait(self, timeout)
    }
}
pub(crate) trait PhysicalPort: Send {
    fn dispatch(
        &self,
        action: HardwareAction,
        permit: ExplicitAction,
    ) -> Result<Box<dyn PhysicalTicket>>;
    fn shutdown_ticket(&self) -> Result<Box<dyn PhysicalTicket>>;
}
impl PhysicalPort for PlatformWorker {
    fn dispatch(
        &self,
        action: HardwareAction,
        permit: ExplicitAction,
    ) -> Result<Box<dyn PhysicalTicket>> {
        Ok(Box::new(PlatformWorker::dispatch(self, action, permit)?))
    }
    fn shutdown_ticket(&self) -> Result<Box<dyn PhysicalTicket>> {
        Ok(Box::new(PlatformWorker::shutdown_ticket(self)?))
    }
}
