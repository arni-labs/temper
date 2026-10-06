//! Event time for exported log records.

use std::time::SystemTime;

use opentelemetry::InstrumentationScope;
use opentelemetry::logs::LogRecord as _;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogProcessor, SdkLogRecord};

/// Gives a log record an event time when it has none.
///
/// The `tracing` bridge leaves the event time unset and the SDK fills in only
/// the observed time. A backend that dates records by their event time then
/// sees a record from 1970 and can drop it while still answering with
/// success. Records that already carry an event time are left untouched.
///
/// Register it before the batch processor: processors run in registration
/// order, and the batch processor copies the record it is given.
#[derive(Debug)]
pub(super) struct EventTimeLogProcessor;

impl LogProcessor for EventTimeLogProcessor {
    fn emit(&self, record: &mut SdkLogRecord, _scope: &InstrumentationScope) {
        if record.timestamp().is_none() {
            // The SDK sets the observed time before it calls any processor.
            // determinism-ok: telemetry timestamp, not a simulation variable.
            let event_time = record.observed_timestamp().unwrap_or_else(SystemTime::now);
            record.set_timestamp(event_time);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown(&self) -> OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use opentelemetry::logs::{Logger as _, LoggerProvider as _};
    use opentelemetry_sdk::logs::SdkLoggerProvider;

    use super::*;

    type Times = (Option<SystemTime>, Option<SystemTime>);

    /// Records the event time and observed time of every record it is given.
    #[derive(Debug, Default)]
    struct RecordTimes(Arc<Mutex<Vec<Times>>>);

    impl LogProcessor for RecordTimes {
        fn emit(&self, record: &mut SdkLogRecord, _scope: &InstrumentationScope) {
            self.0
                .lock()
                .expect("times lock")
                .push((record.timestamp(), record.observed_timestamp()));
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown(&self) -> OTelSdkResult {
            Ok(())
        }
    }

    fn emit_with_event_time(event_time: Option<SystemTime>) -> Times {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = SdkLoggerProvider::builder()
            .with_log_processor(EventTimeLogProcessor)
            .with_log_processor(RecordTimes(Arc::clone(&seen)))
            .build();
        let logger = provider.logger("test");
        let mut record = logger.create_log_record();
        if let Some(event_time) = event_time {
            record.set_timestamp(event_time);
        }
        logger.emit(record);
        seen.lock().expect("times lock")[0]
    }

    #[test]
    fn record_without_event_time_gets_its_observed_time() {
        let (event_time, observed_time) = emit_with_event_time(None);
        assert!(event_time.is_some(), "event time must be set");
        assert_eq!(event_time, observed_time);
    }

    #[test]
    fn record_with_event_time_is_left_untouched() {
        let original = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (event_time, observed_time) = emit_with_event_time(Some(original));
        assert_eq!(event_time, Some(original));
        assert_ne!(observed_time, Some(original));
    }
}
