//! Shared wire contracts published and consumed by the edge runtime.

pub(crate) use elowen_contracts::{
    AvailabilityProbeMessage, AvailabilitySnapshot, DeviceRegistrationTrustProof, DeviceRepository,
    ExecutionIntent, JobApprovalCommand, JobDispatchMessage, JobLifecycleEvent, JobTargetKind,
    RegisterDeviceRequest, RegistrationChallengeResponse, RegistrationTrustIntent,
};
