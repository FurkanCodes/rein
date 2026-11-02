//! Turning a streamed answer into [`Event`]s, whatever the format.
//!
//! Every format streams server-sent events; what each event means differs.
//! [`Decoder`] does the part they share (buffering bytes into events, and
//! knowing when the answer is over), and a [`Translate`] for each format
//! says what its events mean. It's separate from the HTTP code so tests can
//! feed it recorded streams without a network.

use std::collections::VecDeque;

use crate::sse::{SseEvent, SseParser};
use crate::{Error, Event, Result};

/// One format's events, as rein's.
pub trait Translate {
    /// What `sse` means, pushed onto `out`. [`Event::Completed`] or an
    /// error ends the answer: nothing after it is read.
    fn event(&mut self, sse: &SseEvent, out: &mut Vec<Result<Event>>);

    /// The stream closed without the answer having ended. For most formats
    /// that means it was cut off; one that only ends by closing says so here.
    fn closed(&mut self, out: &mut Vec<Result<Event>>) {
        out.push(Err(Error::Interrupted));
    }
}

#[derive(Debug, Default)]
pub struct Decoder<T> {
    parser: SseParser,
    queue: VecDeque<Result<Event>>,
    /// Set once the answer has completed or failed.
    finished: bool,
    translate: T,
}

impl<T: Translate> Decoder<T> {
    pub fn new(translate: T) -> Self {
        Decoder { parser: SseParser::default(), queue: VecDeque::new(), finished: false, translate }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for sse in self.parser.push(bytes) {
            if self.finished {
                break;
            }
            let mut out = Vec::new();
            self.translate.event(&sse, &mut out);
            self.take(out);
        }
    }

    /// Queues `out` up to the event that ends the answer, if any.
    fn take(&mut self, out: Vec<Result<Event>>) {
        for event in out {
            if self.finished {
                break;
            }
            // Completing and every kind of failure end the answer.
            self.finished = matches!(event, Ok(Event::Completed { .. }) | Err(_));
            self.queue.push_back(event);
        }
    }

    pub fn pop(&mut self) -> Option<Result<Event>> {
        self.queue.pop_front()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Call when the stream ends. One that ends before the answer did was
    /// cut off, and that's [`Error::Interrupted`], unless the format ends
    /// answers by closing.
    pub fn finish(&mut self) -> Option<Result<Event>> {
        if !self.finished {
            let mut out = Vec::new();
            self.translate.closed(&mut out);
            self.take(out);
            if !self.finished {
                self.finished = true;
                self.queue.push_back(Err(Error::Interrupted));
            }
        }
        self.pop()
    }

    /// The connection failed: nothing more will come.
    pub fn fail(&mut self) {
        self.finished = true;
    }
}

/// A [`Decoder`] of any format, for the stream to hold.
pub(crate) trait Decode: Send {
    fn push(&mut self, bytes: &[u8]);
    fn pop(&mut self) -> Option<Result<Event>>;
    fn is_finished(&self) -> bool;
    fn finish(&mut self) -> Option<Result<Event>>;
    fn fail(&mut self);
}

impl<T: Translate + Send> Decode for Decoder<T> {
    fn push(&mut self, bytes: &[u8]) {
        Decoder::push(self, bytes)
    }
    fn pop(&mut self) -> Option<Result<Event>> {
        Decoder::pop(self)
    }
    fn is_finished(&self) -> bool {
        Decoder::is_finished(self)
    }
    fn finish(&mut self) -> Option<Result<Event>> {
        Decoder::finish(self)
    }
    fn fail(&mut self) {
        Decoder::fail(self)
    }
}
