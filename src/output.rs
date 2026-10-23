use std::collections::VecDeque;

use serde_json::{Value, json};

const MAX_EVENTS: usize = 256;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Bounded event history included in each tool result, never printed to stdout.
#[derive(Default)]
pub(crate) struct EventLog {
    events: VecDeque<(Value, usize)>,
    bytes: usize,
    dropped: usize,
}

impl EventLog {
    pub fn push(&mut self, event: Value) {
        let size = event.to_string().len();
        if size > MAX_EVENT_BYTES {
            self.dropped += 1;
            return;
        }
        while self.events.len() >= MAX_EVENTS || self.bytes + size > MAX_EVENT_BYTES {
            if let Some((_, size)) = self.events.pop_front() {
                self.bytes -= size;
                self.dropped += 1;
            }
        }
        self.bytes += size;
        self.events.push_back((event, size));
    }

    pub fn attach(self, mut result: Value) -> Value {
        let object = result.as_object_mut().expect("tool results are objects");
        object.insert(
            "events".into(),
            self.events.into_iter().map(|(event, _)| event).collect(),
        );
        object.insert("droppedEvents".into(), json!(self.dropped));
        result
    }
}

pub fn ok(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("ok".into(), json!(true));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_history_is_bounded_and_reports_dropped_events() {
        let mut log = EventLog::default();
        for i in 0..MAX_EVENTS + 5 {
            log.push(json!({"i": i}));
        }
        let result = log.attach(json!({}));
        assert_eq!(result["events"].as_array().unwrap().len(), MAX_EVENTS);
        assert_eq!(result["events"][0]["i"], 5);
        assert_eq!(result["droppedEvents"], 5);
    }
}
