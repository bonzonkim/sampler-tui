use std::fmt;

use sampler_audio::{CaptureCompletion, CaptureSource};
use sampler_core::PadId;

use crate::capture_store::ManagedCaptureId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturePhase {
    Confirm,
    Arming,
    Recording,
    Finalizing,
    ReadyToInstall,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFailureCause {
    WorkerFinalization,
    DeviceRuntime,
    InvalidCapture,
}

impl CaptureFailureCause {
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::WorkerFinalization)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    Unsupported,
    AlreadyActive,
    ActiveCapture {
        source: CaptureSource,
        token: u64,
    },
    NoActiveCapture,
    ZeroCommandToken,
    CommandSourceMismatch {
        expected: CaptureSource,
        received: CaptureSource,
    },
    CommandTokenMismatch {
        expected: u64,
        received: u64,
    },
    CommandRateMismatch {
        expected: u32,
        received: u32,
    },
    TokenExhausted,
    GenerationExhausted,
    ZeroSourceRate,
    ZeroFrameLimit,
    IllegalTransition {
        from: CapturePhase,
        to: CapturePhase,
    },
    CompletionTokenMismatch,
    CompletionTargetMismatch,
    CompletionSourceMismatch,
    CompletionRateMismatch,
    RetryCompletionMissing,
    RetryNotAllowed(CaptureFailureCause),
    Command(sampler_audio::CaptureError),
    InputOpen(String),
    OutputRuntime(String),
    InputRuntime(String),
    DirtySampleDraft(PadId),
    SampleOperationPending(PadId),
    ProjectOperationPending,
    AudioUnavailable,
    EmptyCapture,
    ProjectRevisionExhausted,
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => formatter.write_str("audio capture is unsupported"),
            Self::AlreadyActive => formatter.write_str("a capture is already active"),
            Self::ActiveCapture { source, token } => {
                write!(
                    formatter,
                    "{source:?} capture token {token} is already active"
                )
            }
            Self::NoActiveCapture => formatter.write_str("no capture is active"),
            Self::ZeroCommandToken => formatter.write_str("capture command token must be non-zero"),
            Self::CommandSourceMismatch { expected, received } => write!(
                formatter,
                "capture command source {received:?} does not match active source {expected:?}"
            ),
            Self::CommandTokenMismatch { expected, received } => write!(
                formatter,
                "capture command token {received} does not match active token {expected}"
            ),
            Self::CommandRateMismatch { expected, received } => write!(
                formatter,
                "capture command rate {received} does not match source rate {expected}"
            ),
            Self::TokenExhausted => formatter.write_str("capture tokens are exhausted"),
            Self::GenerationExhausted => formatter.write_str("capture generations are exhausted"),
            Self::ZeroSourceRate => formatter.write_str("capture source rate must be non-zero"),
            Self::ZeroFrameLimit => formatter.write_str("capture frame limit must be non-zero"),
            Self::IllegalTransition { from, to } => {
                write!(
                    formatter,
                    "capture cannot transition from {from:?} to {to:?}"
                )
            }
            Self::CompletionTokenMismatch => {
                formatter.write_str("capture completion token does not match")
            }
            Self::CompletionTargetMismatch => {
                formatter.write_str("capture completion target does not match")
            }
            Self::CompletionSourceMismatch => {
                formatter.write_str("capture completion source does not match")
            }
            Self::CompletionRateMismatch => {
                formatter.write_str("capture completion source rate does not match")
            }
            Self::RetryCompletionMissing => {
                formatter.write_str("failed capture has no completion to retry")
            }
            Self::RetryNotAllowed(cause) => {
                write!(formatter, "capture failure {cause:?} requires a fresh take")
            }
            Self::Command(error) => error.fmt(formatter),
            Self::InputOpen(error) => write!(formatter, "could not open input capture: {error}"),
            Self::OutputRuntime(error) => write!(formatter, "output capture failed: {error}"),
            Self::InputRuntime(error) => write!(formatter, "input capture failed: {error}"),
            Self::DirtySampleDraft(pad) => {
                write!(formatter, "pad {pad:?} has an uncommitted sample draft")
            }
            Self::SampleOperationPending(pad) => {
                write!(formatter, "pad {pad:?} has pending sample work")
            }
            Self::ProjectOperationPending => formatter.write_str("a project operation is pending"),
            Self::AudioUnavailable => formatter.write_str("audio device is unavailable"),
            Self::EmptyCapture => formatter.write_str("capture contains no frames"),
            Self::ProjectRevisionExhausted => formatter.write_str("project revision is exhausted"),
        }
    }
}

impl std::error::Error for CaptureError {}

#[derive(Debug)]
pub struct CaptureCompletionFailure {
    error: CaptureError,
    completion: CaptureCompletion,
}

impl CaptureCompletionFailure {
    pub fn error(&self) -> CaptureError {
        self.error.clone()
    }

    pub fn into_completion(self) -> CaptureCompletion {
        self.completion
    }
}

struct ActiveCapture {
    token: u64,
    generation: u64,
    target: PadId,
    source: CaptureSource,
    source_rate: u32,
    max_frames: usize,
    phase: CapturePhase,
    completion: Option<CaptureCompletion>,
    failure: Option<CaptureFailure>,
    managed_capture_id: Option<ManagedCaptureId>,
}

struct CaptureFailure {
    cause: CaptureFailureCause,
    message: String,
}

#[derive(Default)]
pub struct CaptureSession {
    last_token: u64,
    last_generation: u64,
    active: Option<ActiveCapture>,
}

impl CaptureSession {
    pub fn begin(
        &mut self,
        source: CaptureSource,
        target: PadId,
        source_rate: u32,
        max_frames: usize,
    ) -> Result<(), CaptureError> {
        if self.active.is_some() {
            return Err(CaptureError::AlreadyActive);
        }
        if source_rate == 0 {
            return Err(CaptureError::ZeroSourceRate);
        }
        if max_frames == 0 {
            return Err(CaptureError::ZeroFrameLimit);
        }
        let token = self
            .last_token
            .checked_add(1)
            .ok_or(CaptureError::TokenExhausted)?;
        let generation = self
            .last_generation
            .checked_add(1)
            .ok_or(CaptureError::GenerationExhausted)?;

        self.last_token = token;
        self.last_generation = generation;
        self.active = Some(ActiveCapture {
            token,
            generation,
            target,
            source,
            source_rate,
            max_frames,
            phase: CapturePhase::Confirm,
            completion: None,
            failure: None,
            managed_capture_id: None,
        });
        Ok(())
    }

    pub fn mark_arming(&mut self) -> Result<(), CaptureError> {
        self.transition(CapturePhase::Confirm, CapturePhase::Arming)
    }

    pub fn mark_recording(&mut self) -> Result<(), CaptureError> {
        self.transition(CapturePhase::Arming, CapturePhase::Recording)
    }

    pub fn accept_completion(
        &mut self,
        completion: CaptureCompletion,
    ) -> Result<(), CaptureCompletionFailure> {
        let Some(active) = self.active.as_mut() else {
            return Err(CaptureCompletionFailure {
                error: CaptureError::NoActiveCapture,
                completion,
            });
        };
        if active.phase != CapturePhase::Recording {
            return Err(CaptureCompletionFailure {
                error: CaptureError::IllegalTransition {
                    from: active.phase,
                    to: CapturePhase::Finalizing,
                },
                completion,
            });
        }
        let mismatch = if completion.token != active.token {
            Some(CaptureError::CompletionTokenMismatch)
        } else if completion.target != active.target {
            Some(CaptureError::CompletionTargetMismatch)
        } else if completion.source != active.source {
            Some(CaptureError::CompletionSourceMismatch)
        } else if completion.sample_rate != active.source_rate {
            Some(CaptureError::CompletionRateMismatch)
        } else {
            None
        };
        if let Some(error) = mismatch {
            return Err(CaptureCompletionFailure { error, completion });
        }
        active.completion = Some(completion);
        active.phase = CapturePhase::Finalizing;
        Ok(())
    }

    pub fn mark_ready_to_install(&mut self) -> Result<(), CaptureError> {
        self.transition(CapturePhase::Finalizing, CapturePhase::ReadyToInstall)
    }

    pub(crate) fn set_managed_capture_id(
        &mut self,
        id: Option<ManagedCaptureId>,
    ) -> Result<(), CaptureError> {
        let active = self.active.as_mut().ok_or(CaptureError::NoActiveCapture)?;
        active.managed_capture_id = id;
        Ok(())
    }

    pub fn mark_failed(&mut self, message: impl Into<String>) -> Result<(), CaptureError> {
        self.mark_failed_with_cause(CaptureFailureCause::InvalidCapture, message)
    }

    pub(crate) fn mark_failed_with_cause(
        &mut self,
        cause: CaptureFailureCause,
        message: impl Into<String>,
    ) -> Result<(), CaptureError> {
        let active = self.active.as_mut().ok_or(CaptureError::NoActiveCapture)?;
        active.phase = CapturePhase::Failed;
        active.failure = Some(CaptureFailure {
            cause,
            message: message.into(),
        });
        Ok(())
    }

    pub fn retry_finalization_with_next_generation(&mut self) -> Result<u64, CaptureError> {
        let active = self.active.as_ref().ok_or(CaptureError::NoActiveCapture)?;
        if active.phase != CapturePhase::Failed {
            return Err(CaptureError::IllegalTransition {
                from: active.phase,
                to: CapturePhase::Finalizing,
            });
        }
        let cause = active
            .failure
            .as_ref()
            .map_or(CaptureFailureCause::InvalidCapture, |failure| failure.cause);
        if !cause.is_retryable() {
            return Err(CaptureError::RetryNotAllowed(cause));
        }
        if active.completion.is_none() {
            return Err(CaptureError::RetryCompletionMissing);
        }
        let generation = self
            .last_generation
            .checked_add(1)
            .ok_or(CaptureError::GenerationExhausted)?;
        let active = self
            .active
            .as_mut()
            .expect("active capture was validated before mutation");
        self.last_generation = generation;
        active.generation = generation;
        active.phase = CapturePhase::Finalizing;
        active.failure = None;
        active.managed_capture_id = None;
        Ok(generation)
    }

    pub(crate) fn advance_finalization_generation(&mut self) -> Result<u64, CaptureError> {
        let active = self.active.as_ref().ok_or(CaptureError::NoActiveCapture)?;
        if !matches!(
            active.phase,
            CapturePhase::Finalizing | CapturePhase::ReadyToInstall
        ) {
            return Err(CaptureError::IllegalTransition {
                from: active.phase,
                to: CapturePhase::Finalizing,
            });
        }
        let generation = self
            .last_generation
            .checked_add(1)
            .ok_or(CaptureError::GenerationExhausted)?;
        let active = self
            .active
            .as_mut()
            .expect("active capture was validated before mutation");
        self.last_generation = generation;
        active.generation = generation;
        active.phase = CapturePhase::Finalizing;
        active.failure = None;
        active.managed_capture_id = None;
        Ok(generation)
    }

    pub(crate) fn take_completion_stereo(&mut self) -> Option<Vec<f32>> {
        self.active
            .as_mut()?
            .completion
            .as_mut()
            .map(|completion| std::mem::take(&mut completion.stereo))
    }

    pub fn discard(&mut self) -> Result<Option<CaptureCompletion>, CaptureError> {
        let active = self.active.take().ok_or(CaptureError::NoActiveCapture)?;
        Ok(active.completion)
    }

    pub fn token(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.token)
    }

    pub fn generation(&self) -> Option<u64> {
        self.active.as_ref().map(|active| active.generation)
    }

    pub fn target(&self) -> Option<PadId> {
        self.active.as_ref().map(|active| active.target)
    }

    pub fn source(&self) -> Option<CaptureSource> {
        self.active.as_ref().map(|active| active.source)
    }

    pub fn source_rate(&self) -> Option<u32> {
        self.active.as_ref().map(|active| active.source_rate)
    }

    pub fn max_frames(&self) -> Option<usize> {
        self.active.as_ref().map(|active| active.max_frames)
    }

    pub fn phase(&self) -> Option<CapturePhase> {
        self.active.as_ref().map(|active| active.phase)
    }

    pub fn completion(&self) -> Option<&CaptureCompletion> {
        self.active
            .as_ref()
            .and_then(|active| active.completion.as_ref())
    }

    pub fn failure(&self) -> Option<&str> {
        self.active
            .as_ref()
            .and_then(|active| active.failure.as_ref())
            .map(|failure| failure.message.as_str())
    }

    pub fn failure_cause(&self) -> Option<CaptureFailureCause> {
        self.active
            .as_ref()
            .and_then(|active| active.failure.as_ref())
            .map(|failure| failure.cause)
    }

    pub fn failure_is_retryable(&self) -> bool {
        self.failure_cause()
            .is_some_and(CaptureFailureCause::is_retryable)
    }

    pub fn managed_capture_id(&self) -> Option<ManagedCaptureId> {
        self.active
            .as_ref()
            .and_then(|active| active.managed_capture_id)
    }

    #[cfg(test)]
    pub(crate) const fn sequence_for_test(&self) -> (u64, u64) {
        (self.last_token, self.last_generation)
    }

    fn transition(
        &mut self,
        expected: CapturePhase,
        next: CapturePhase,
    ) -> Result<(), CaptureError> {
        let active = self.active.as_mut().ok_or(CaptureError::NoActiveCapture)?;
        if active.phase != expected {
            return Err(CaptureError::IllegalTransition {
                from: active.phase,
                to: next,
            });
        }
        active.phase = next;
        Ok(())
    }

    #[cfg(test)]
    fn with_last_ids(last_token: u64, last_generation: u64) -> Self {
        Self {
            last_token,
            last_generation,
            active: None,
        }
    }

    #[cfg(test)]
    const fn last_ids(&self) -> (u64, u64) {
        (self.last_token, self.last_generation)
    }
}

#[cfg(test)]
mod tests {
    use sampler_audio::{CaptureCompletion, CaptureSource};
    use sampler_core::{BankId, PadId};

    use super::{CaptureError, CaptureFailureCause, CapturePhase, CaptureSession};

    fn completion(session: &CaptureSession, source: CaptureSource) -> CaptureCompletion {
        CaptureCompletion {
            token: session.token().unwrap(),
            target: session.target().unwrap(),
            source,
            sample_rate: session.source_rate().unwrap(),
            stereo: vec![0.25, -0.25],
            hard_limit: false,
            peak: 0.25,
        }
    }

    #[test]
    fn one_cross_source_capture_preserves_selected_context_and_monotonic_nonzero_ids() {
        let target = PadId::new(BankId::new(0).unwrap(), 9).unwrap();
        let mut session = CaptureSession::default();

        session
            .begin(CaptureSource::Resample, target, 48_000, 96_000)
            .unwrap();
        assert_eq!(session.token(), Some(1));
        assert_eq!(session.generation(), Some(1));
        assert_eq!(session.target(), Some(target));
        assert_eq!(session.source(), Some(CaptureSource::Resample));
        assert_eq!(session.source_rate(), Some(48_000));
        assert_eq!(session.max_frames(), Some(96_000));
        assert_eq!(session.phase(), Some(CapturePhase::Confirm));

        assert_eq!(
            session.begin(CaptureSource::Input, PadId::first(), 44_100, 44_100),
            Err(CaptureError::AlreadyActive)
        );
        assert_eq!((session.token(), session.generation()), (Some(1), Some(1)));

        session.discard().unwrap();
        session
            .begin(CaptureSource::Input, PadId::first(), 44_100, 44_100)
            .unwrap();
        assert_eq!((session.token(), session.generation()), (Some(2), Some(2)));
    }

    #[test]
    fn identifier_exhaustion_is_typed_and_does_not_partially_advance() {
        let mut token_exhausted = CaptureSession::with_last_ids(u64::MAX, 7);
        assert_eq!(
            token_exhausted.begin(CaptureSource::Resample, PadId::first(), 48_000, 1),
            Err(CaptureError::TokenExhausted)
        );
        assert_eq!(token_exhausted.last_ids(), (u64::MAX, 7));

        let mut generation_exhausted = CaptureSession::with_last_ids(7, u64::MAX);
        assert_eq!(
            generation_exhausted.begin(CaptureSource::Input, PadId::first(), 44_100, 1),
            Err(CaptureError::GenerationExhausted)
        );
        assert_eq!(generation_exhausted.last_ids(), (7, u64::MAX));
    }

    #[test]
    fn legal_transitions_retain_only_an_exact_completion_candidate() {
        let mut session = CaptureSession::default();
        session
            .begin(CaptureSource::Resample, PadId::first(), 48_000, 16)
            .unwrap();
        assert_eq!(
            session.mark_recording(),
            Err(CaptureError::IllegalTransition {
                from: CapturePhase::Confirm,
                to: CapturePhase::Recording,
            })
        );

        session.mark_arming().unwrap();
        session.mark_recording().unwrap();
        let wrong = completion(&session, CaptureSource::Input);
        let failure = session.accept_completion(wrong).unwrap_err();
        assert_eq!(failure.error(), CaptureError::CompletionSourceMismatch);
        assert_eq!(failure.into_completion().source, CaptureSource::Input);
        assert!(session.completion().is_none());
        assert_eq!(session.phase(), Some(CapturePhase::Recording));

        session
            .accept_completion(completion(&session, CaptureSource::Resample))
            .unwrap();
        assert_eq!(session.phase(), Some(CapturePhase::Finalizing));
        assert_eq!(session.completion().unwrap().stereo, [0.25, -0.25]);
        session.mark_ready_to_install().unwrap();
        assert_eq!(session.phase(), Some(CapturePhase::ReadyToInstall));
        assert_eq!(
            session.mark_recording(),
            Err(CaptureError::IllegalTransition {
                from: CapturePhase::ReadyToInstall,
                to: CapturePhase::Recording,
            })
        );
    }

    #[test]
    fn every_completion_identity_mismatch_returns_the_exact_vec_and_preserves_state() {
        #[derive(Clone, Copy)]
        enum Mismatch {
            Token,
            Target,
            Source,
            Rate,
        }

        for mismatch in [
            Mismatch::Token,
            Mismatch::Target,
            Mismatch::Source,
            Mismatch::Rate,
        ] {
            let mut session = CaptureSession::default();
            session
                .begin(CaptureSource::Resample, PadId::first(), 48_000, 16)
                .unwrap();
            session.mark_arming().unwrap();
            session.mark_recording().unwrap();
            let mut candidate = completion(&session, CaptureSource::Resample);
            let expected_error = match mismatch {
                Mismatch::Token => {
                    candidate.token += 1;
                    CaptureError::CompletionTokenMismatch
                }
                Mismatch::Target => {
                    candidate.target = PadId::new(BankId::new(0).unwrap(), 1).unwrap();
                    CaptureError::CompletionTargetMismatch
                }
                Mismatch::Source => {
                    candidate.source = CaptureSource::Input;
                    CaptureError::CompletionSourceMismatch
                }
                Mismatch::Rate => {
                    candidate.sample_rate = 44_100;
                    CaptureError::CompletionRateMismatch
                }
            };
            let pointer = candidate.stereo.as_ptr();
            let before = (
                session.token(),
                session.generation(),
                session.target(),
                session.source(),
                session.source_rate(),
                session.max_frames(),
                session.phase(),
            );

            let failure = session.accept_completion(candidate).unwrap_err();

            assert_eq!(failure.error(), expected_error);
            let returned = failure.into_completion();
            assert_eq!(returned.stereo.as_ptr(), pointer);
            assert_eq!(
                (
                    session.token(),
                    session.generation(),
                    session.target(),
                    session.source(),
                    session.source_rate(),
                    session.max_frames(),
                    session.phase(),
                ),
                before
            );
            assert!(session.completion().is_none());
        }
    }

    #[test]
    fn failed_retry_checks_generation_before_atomic_phase_and_generation_change() {
        let mut session = CaptureSession::with_last_ids(0, 6);
        session
            .begin(CaptureSource::Input, PadId::first(), 44_100, 16)
            .unwrap();
        session.mark_arming().unwrap();
        assert_eq!(
            session.retry_finalization_with_next_generation(),
            Err(CaptureError::IllegalTransition {
                from: CapturePhase::Arming,
                to: CapturePhase::Finalizing,
            })
        );
        assert_eq!(
            (session.generation(), session.phase()),
            (Some(7), Some(CapturePhase::Arming))
        );

        session.mark_recording().unwrap();
        session
            .accept_completion(completion(&session, CaptureSource::Input))
            .unwrap();
        session
            .mark_failed_with_cause(CaptureFailureCause::WorkerFinalization, "encode failed")
            .unwrap();
        assert_eq!(session.retry_finalization_with_next_generation(), Ok(8));
        assert_eq!(
            (session.generation(), session.phase(), session.failure()),
            (Some(8), Some(CapturePhase::Finalizing), None)
        );

        let mut without_completion = CaptureSession::default();
        without_completion
            .begin(CaptureSource::Input, PadId::first(), 44_100, 16)
            .unwrap();
        without_completion
            .mark_failed_with_cause(CaptureFailureCause::DeviceRuntime, "device lost")
            .unwrap();
        assert_eq!(
            without_completion.retry_finalization_with_next_generation(),
            Err(CaptureError::RetryNotAllowed(
                CaptureFailureCause::DeviceRuntime
            ))
        );
        assert_eq!(
            (
                without_completion.generation(),
                without_completion.phase(),
                without_completion.failure(),
            ),
            (Some(1), Some(CapturePhase::Failed), Some("device lost"))
        );
    }

    #[test]
    fn failed_retry_overflow_preserves_generation_phase_completion_and_failure() {
        let mut session = CaptureSession::with_last_ids(0, u64::MAX - 1);
        session
            .begin(CaptureSource::Input, PadId::first(), 44_100, 16)
            .unwrap();
        session.mark_arming().unwrap();
        session.mark_recording().unwrap();
        session
            .accept_completion(completion(&session, CaptureSource::Input))
            .unwrap();
        session
            .mark_failed_with_cause(CaptureFailureCause::WorkerFinalization, "retryable")
            .unwrap();
        let completion_pointer = session.completion().unwrap().stereo.as_ptr();

        assert_eq!(
            session.retry_finalization_with_next_generation(),
            Err(CaptureError::GenerationExhausted)
        );
        assert_eq!(session.last_ids(), (1, u64::MAX));
        assert_eq!(session.generation(), Some(u64::MAX));
        assert_eq!(session.phase(), Some(CapturePhase::Failed));
        assert_eq!(session.failure(), Some("retryable"));
        assert_eq!(
            session.completion().unwrap().stereo.as_ptr(),
            completion_pointer
        );
    }

    #[test]
    fn capture_app_owns_the_isolated_session_model() {
        let mut app = crate::App::without_audio("offline");
        assert_eq!(app.capture_session().phase(), None);

        app.capture_session_mut()
            .begin(CaptureSource::Input, PadId::first(), 44_100, 88_200)
            .unwrap();

        assert_eq!(app.capture_session().source(), Some(CaptureSource::Input));
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Confirm));
    }
}
