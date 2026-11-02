//! Pure bounded command admission/ownership. The platform must report actual completion.
use crate::thermal::{ApplyStatus, Epoch, PowerCapability, PowerCommand, PowerEnvelope};
use crate::{CoreError, CoreResult};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueuedCommand {
    pub id: u64,
    pub command: PowerCommand,
    pub deadline_ms: u64,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Admission {
    Idle,
    Busy,
    Started(QueuedCommand),
    Rejected { id: u64 },
}
#[derive(Debug, Clone, Copy)]
struct Active {
    request: QueuedCommand,
    unknown: bool,
}
#[derive(Debug, Default)]
pub struct CommandQueue {
    waiting: VecDeque<QueuedCommand>,
    active: Option<Active>,
    next_id: u64,
}
impl CommandQueue {
    pub const CAPACITY: usize = 8;
    pub fn len(&self) -> usize {
        self.waiting.len() + usize::from(self.active.is_some())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn owns_physical_io(&self) -> bool {
        self.active.is_some()
    }
    pub fn enqueue(
        &mut self,
        command: PowerCommand,
        capability: PowerCapability,
        now: u64,
        epoch: Epoch,
        timeout_ms: u64,
    ) -> CoreResult<u64> {
        if timeout_ms == 0 || timeout_ms > i32::MAX as u64 {
            return Err(CoreError::invalid("bounded command timeout"));
        }
        if !command.is_current(capability, now, epoch) {
            return Err(CoreError::Superseded);
        }
        if self.len() >= Self::CAPACITY {
            return Err(CoreError::Capacity("hardware command queue (8)".into()));
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| CoreError::Capacity("command identity exhausted".into()))?;
        let request = QueuedCommand {
            id: self.next_id,
            command,
            deadline_ms: now.saturating_add(timeout_ms).min(command.expires_at_ms),
        };
        self.waiting.push_back(request);
        Ok(request.id)
    }
    pub fn poll(&mut self, capability: PowerCapability, now: u64, epoch: Epoch) -> Admission {
        if self.active.is_some() {
            return Admission::Busy;
        }
        let Some(request) = self.waiting.pop_front() else {
            return Admission::Idle;
        };
        if now >= request.deadline_ms || !request.command.is_current(capability, now, epoch) {
            return Admission::Rejected { id: request.id };
        }
        self.active = Some(Active {
            request,
            unknown: false,
        });
        Admission::Started(request)
    }
    pub fn stop_waiting(&mut self, id: u64) -> CoreResult<ApplyStatus> {
        if let Some(active) = self.active.as_mut().filter(|a| a.request.id == id) {
            active.unknown = true;
            return Ok(ApplyStatus::UnknownCompletion);
        }
        if let Some(index) = self.waiting.iter().position(|q| q.id == id) {
            self.waiting.remove(index);
            return Ok(ApplyStatus::Rejected);
        }
        Err(CoreError::Superseded)
    }
    pub fn finish_physical(
        &mut self,
        id: u64,
        readback: CoreResult<PowerEnvelope>,
        capability: PowerCapability,
        epoch: Epoch,
    ) -> CoreResult<ApplyStatus> {
        let active = self
            .active
            .filter(|a| a.request.id == id)
            .ok_or(CoreError::Superseded)?;
        self.active = None;
        if active.unknown {
            return Ok(ApplyStatus::UnknownCompletion);
        }
        Ok(
            if readback.is_ok_and(|actual| {
                actual == active.request.command.target && capability.is_aligned(actual)
            }) && epoch == active.request.command.epoch
            {
                ApplyStatus::Verified
            } else {
                ApplyStatus::Failed
            },
        )
    }
}
