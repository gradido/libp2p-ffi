//! The queue between the node's threads and whoever calls `lp2p_poll`, and the record format.
//!
//! A record is an `lp2p_event` header followed by its data, padded to a multiple of 8. The queue is
//! bounded in bytes. What does not fit is counted and reported as one `LP2P_EV_OVERFLOW` record
//! ahead of the next events, so a caller always learns that it missed something.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::abi::*;

pub struct Record(Vec<u8>);

pub struct EventBuilder {
    header: lp2p_event,
}

impl EventBuilder {
    pub fn new(kind: u16) -> Self {
        EventBuilder {
            header: lp2p_event {
                r#type: kind,
                ..Default::default()
            },
        }
    }
    pub fn id(mut self, id: u64) -> Self {
        self.header.id = id;
        self
    }
    pub fn flags(mut self, flags: u16) -> Self {
        self.header.flags = flags;
        self
    }
    pub fn group(mut self, group: lp2p_key) -> Self {
        self.header.group = group;
        self
    }
    pub fn node(mut self, node: lp2p_key) -> Self {
        self.header.node = node;
        self
    }
    pub fn protocol(mut self, protocol: u16) -> Self {
        self.header.protocol = protocol;
        self
    }
    pub fn reason(mut self, reason: u16) -> Self {
        self.header.reason = reason;
        self
    }

    pub fn data(self, data: &[u8]) -> Record {
        let mut h = self.header;
        let unpadded = LP2P_EVENT_HEADER_BYTES + data.len();
        let size = unpadded.div_ceil(8) * 8;
        h.size = size as u32;
        h.data_len = data.len() as u32;
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&h.r#type.to_ne_bytes());
        out.extend_from_slice(&h.flags.to_ne_bytes());
        out.extend_from_slice(&h.size.to_ne_bytes());
        out.extend_from_slice(&h.id.to_ne_bytes());
        out.extend_from_slice(&h.group);
        out.extend_from_slice(&h.node);
        out.extend_from_slice(&h.protocol.to_ne_bytes());
        out.extend_from_slice(&h.reason.to_ne_bytes());
        out.extend_from_slice(&h.data_len.to_ne_bytes());
        debug_assert_eq!(out.len(), LP2P_EVENT_HEADER_BYTES);
        out.extend_from_slice(data);
        out.resize(size, 0);
        Record(out)
    }

    pub fn build(self) -> Record {
        self.data(&[])
    }
}

impl Record {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

struct Inner {
    records: VecDeque<Record>,
    bytes: usize,
    dropped: u64,
    closed: Option<i32>,
}

pub struct EventQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
    max_bytes: usize,
}

impl EventQueue {
    pub fn new(max_bytes: usize) -> Self {
        EventQueue {
            inner: Mutex::new(Inner {
                records: VecDeque::new(),
                bytes: 0,
                dropped: 0,
                closed: None,
            }),
            ready: Condvar::new(),
            max_bytes,
        }
    }

    pub fn push(&self, record: Record) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.bytes + record.0.len() > self.max_bytes {
            inner.dropped += 1;
        } else {
            inner.bytes += record.0.len();
            inner.records.push_back(record);
        }
        drop(inner);
        self.ready.notify_all();
    }

    /// Wakes every poll and makes the next ones answer @p status once the queue is drained.
    pub fn close(&self, status: i32) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.closed.get_or_insert(status);
        drop(inner);
        self.ready.notify_all();
    }

    pub fn dropped(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).dropped
    }

    /// Whole records into @p buf: the bytes written, or a negative status.
    pub fn poll(&self, buf: &mut [u8], timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while inner.records.is_empty() && inner.dropped == 0 && inner.closed.is_none() {
            let now = Instant::now();
            if now >= deadline {
                return 0;
            }
            inner = self
                .ready
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }

        let mut written = 0usize;
        if inner.dropped > 0 {
            let overflow = EventBuilder::new(LP2P_EV_OVERFLOW).id(inner.dropped).build();
            if overflow.0.len() > buf.len() {
                return LP2P_ERR_BUFFER_TOO_SMALL;
            }
            buf[..overflow.0.len()].copy_from_slice(&overflow.0);
            written = overflow.0.len();
            inner.dropped = 0;
        }
        while let Some(front) = inner.records.front() {
            let len = front.0.len();
            if written + len > buf.len() {
                if written == 0 {
                    // The record stays queued; a larger buffer gets it.
                    return LP2P_ERR_BUFFER_TOO_SMALL;
                }
                break;
            }
            buf[written..written + len].copy_from_slice(&front.0);
            written += len;
            inner.bytes -= len;
            inner.records.pop_front();
        }
        if written == 0
            && let Some(status) = inner.closed
        {
            return status;
        }
        written as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(bytes: &[u8]) -> lp2p_event {
        lp2p_event {
            r#type: u16::from_ne_bytes([bytes[0], bytes[1]]),
            size: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            id: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            data_len: u32::from_ne_bytes(bytes[84..88].try_into().unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn records_are_padded_and_whole() {
        let q = EventQueue::new(1 << 16);
        q.push(EventBuilder::new(LP2P_EV_LISTENING).data(b"abc"));
        q.push(EventBuilder::new(LP2P_EV_LISTENING).data(b"defgh"));
        let mut buf = [0u8; 100];
        let n = q.poll(&mut buf, Duration::ZERO);
        assert_eq!(n, 96, "only the first record fits");
        let h = header(&buf);
        assert_eq!((h.r#type, h.size, h.data_len), (LP2P_EV_LISTENING, 96, 3));
        let mut small = [0u8; 50];
        assert_eq!(q.poll(&mut small, Duration::ZERO), LP2P_ERR_BUFFER_TOO_SMALL);
        assert_eq!(q.poll(&mut buf, Duration::ZERO), 96);
        assert_eq!(q.poll(&mut buf, Duration::from_millis(10)), 0);
    }

    #[test]
    fn overflow_is_reported_before_the_next_events() {
        let q = EventQueue::new(96);
        q.push(EventBuilder::new(LP2P_EV_LISTENING).data(b"a"));
        q.push(EventBuilder::new(LP2P_EV_LISTENING).data(b"b"));
        q.push(EventBuilder::new(LP2P_EV_LISTENING).data(b"c"));
        let mut buf = [0u8; 1024];
        let n = q.poll(&mut buf, Duration::ZERO);
        assert_eq!(n, 88 + 96);
        let h = header(&buf);
        assert_eq!((h.r#type, h.id), (LP2P_EV_OVERFLOW, 2));
    }

    #[test]
    fn a_closed_queue_answers_its_status_once_drained() {
        let q = EventQueue::new(1024);
        q.push(EventBuilder::new(LP2P_EV_LISTENING).build());
        q.close(LP2P_ERR_SHUT_DOWN);
        let mut buf = [0u8; 1024];
        assert_eq!(q.poll(&mut buf, Duration::from_secs(5)), 88);
        assert_eq!(q.poll(&mut buf, Duration::from_secs(5)), LP2P_ERR_SHUT_DOWN);
    }
}
