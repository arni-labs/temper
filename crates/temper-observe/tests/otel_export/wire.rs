//! Minimal reader for the OTLP protobuf requests the exporters send.
//!
//! Decodes only the fields the tests look at, straight from the protobuf
//! wire format, so the tests need no protobuf dependency.

use std::collections::BTreeMap;

/// Attribute keys mapped to their values rendered as text.
pub type Attributes = BTreeMap<String, String>;

/// One exported span with the resource and scope it was sent under.
#[derive(Clone, Debug)]
pub struct Span {
    pub resource: Attributes,
    pub scope: String,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
    pub name: String,
    pub kind: u64,
    pub flags: u32,
    pub attributes: Attributes,
    pub events: Vec<String>,
    pub status_code: u64,
}

/// One exported log record with the resource and scope it was sent under.
#[derive(Clone, Debug)]
pub struct LogRecord {
    pub resource: Attributes,
    pub scope: String,
    pub time_unix_nano: u64,
    pub observed_time_unix_nano: u64,
    pub severity_number: u64,
    pub severity_text: String,
    pub body: String,
    pub attributes: Attributes,
    pub trace_id: String,
    pub span_id: String,
}

/// One exported metric with the resource and scope it was sent under.
#[derive(Clone, Debug)]
pub struct Metric {
    pub resource: Attributes,
    pub scope: String,
    pub name: String,
    pub description: String,
    pub unit: String,
    pub kind: &'static str,
    /// Data point attributes and the point's value rendered as text.
    pub points: Vec<(Attributes, String)>,
}

/// Decode the body of a request to `/v1/traces`.
pub fn spans(body: &[u8]) -> Vec<Span> {
    let mut out = Vec::new();
    for resource_spans in Message::parse(body).repeated(1) {
        let resource = resource_attributes(&resource_spans);
        for scope_spans in resource_spans.repeated(2) {
            let scope = scope_spans.message(1).string(1);
            for span in scope_spans.repeated(2) {
                out.push(Span {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    trace_id: hex(span.bytes(1)),
                    span_id: hex(span.bytes(2)),
                    parent_span_id: hex(span.bytes(4)),
                    name: span.string(5),
                    kind: span.varint(6),
                    flags: span.fixed32(16),
                    attributes: attributes(&span, 9),
                    events: span.repeated(11).iter().map(|e| e.string(2)).collect(),
                    status_code: span.message(15).varint(3),
                });
            }
        }
    }
    out
}

/// Decode the body of a request to `/v1/logs`.
pub fn logs(body: &[u8]) -> Vec<LogRecord> {
    let mut out = Vec::new();
    for resource_logs in Message::parse(body).repeated(1) {
        let resource = resource_attributes(&resource_logs);
        for scope_logs in resource_logs.repeated(2) {
            let scope = scope_logs.message(1).string(1);
            for record in scope_logs.repeated(2) {
                out.push(LogRecord {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    time_unix_nano: record.fixed64(1),
                    observed_time_unix_nano: record.fixed64(11),
                    severity_number: record.varint(2),
                    severity_text: record.string(3),
                    body: any_value(&record.message(5)),
                    attributes: attributes(&record, 6),
                    trace_id: hex(record.bytes(9)),
                    span_id: hex(record.bytes(10)),
                });
            }
        }
    }
    out
}

/// Decode the body of a request to `/v1/metrics`.
pub fn metrics(body: &[u8]) -> Vec<Metric> {
    let mut out = Vec::new();
    for resource_metrics in Message::parse(body).repeated(1) {
        let resource = resource_attributes(&resource_metrics);
        for scope_metrics in resource_metrics.repeated(2) {
            let scope = scope_metrics.message(1).string(1);
            for metric in scope_metrics.repeated(2) {
                // Metric.data is a oneof: gauge = 5, sum = 7, histogram = 9.
                let (kind, data) = [(5, "gauge"), (7, "sum"), (9, "histogram")]
                    .into_iter()
                    .find(|(field, _)| metric.has(*field))
                    .map(|(field, kind)| (kind, metric.message(field)))
                    .unwrap_or(("other", Message::default()));
                out.push(Metric {
                    resource: resource.clone(),
                    scope: scope.clone(),
                    name: metric.string(1),
                    description: metric.string(2),
                    unit: metric.string(3),
                    kind,
                    points: data
                        .repeated(1)
                        .iter()
                        .map(|point| data_point(kind, point))
                        .collect(),
                });
            }
        }
    }
    out
}

fn data_point(kind: &str, point: &Message<'_>) -> (Attributes, String) {
    if kind == "histogram" {
        // HistogramDataPoint: attributes = 9, count = 2.
        return (attributes(point, 9), format!("count={}", point.fixed64(2)));
    }
    // NumberDataPoint: attributes = 7, as_double = 4, as_int = 6.
    let value = if point.has(4) {
        f64::from_bits(point.fixed64(4)).to_string()
    } else {
        (point.fixed64(6) as i64).to_string()
    };
    (attributes(point, 7), value)
}

fn resource_attributes(container: &Message<'_>) -> Attributes {
    attributes(&container.message(1), 1)
}

fn attributes(message: &Message<'_>, field: u32) -> Attributes {
    message
        .repeated(field)
        .iter()
        .map(|pair| (pair.string(1), any_value(&pair.message(2))))
        .collect()
}

/// Render an `AnyValue` as text.
fn any_value(value: &Message<'_>) -> String {
    match value.fields.first() {
        None => String::new(),
        Some((1, Wire::Bytes(text))) => String::from_utf8_lossy(text).into_owned(),
        Some((2, Wire::Varint(flag))) => (*flag != 0).to_string(),
        Some((3, Wire::Varint(int))) => (*int as i64).to_string(),
        Some((4, Wire::Fixed64(bits))) => f64::from_bits(*bits).to_string(),
        Some((5, Wire::Bytes(array))) => {
            let items: Vec<String> = Message::parse(array)
                .repeated(1)
                .iter()
                .map(any_value)
                .collect();
            format!("[{}]", items.join(", "))
        }
        Some((6, Wire::Bytes(list))) => {
            let pairs: Vec<String> = attributes(&Message::parse(list), 1)
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            format!("{{{}}}", pairs.join(", "))
        }
        Some((7, Wire::Bytes(bytes))) => hex(bytes),
        Some((field, _)) => panic!("unexpected AnyValue field {field}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug)]
enum Wire<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
}

/// The fields of one protobuf message, in wire order.
#[derive(Debug, Default)]
struct Message<'a> {
    fields: Vec<(u32, Wire<'a>)>,
}

impl<'a> Message<'a> {
    fn parse(mut buf: &'a [u8]) -> Self {
        let mut fields = Vec::new();
        while !buf.is_empty() {
            let key = read_varint(&mut buf);
            let value = match key & 7 {
                0 => Wire::Varint(read_varint(&mut buf)),
                1 => Wire::Fixed64(u64::from_le_bytes(take::<8>(&mut buf))),
                2 => {
                    let len = read_varint(&mut buf) as usize;
                    assert!(len <= buf.len(), "truncated protobuf field");
                    let (head, rest) = buf.split_at(len);
                    buf = rest;
                    Wire::Bytes(head)
                }
                5 => Wire::Fixed32(u32::from_le_bytes(take::<4>(&mut buf))),
                other => panic!("unsupported protobuf wire type {other}"),
            };
            fields.push(((key >> 3) as u32, value));
        }
        Self { fields }
    }

    fn has(&self, field: u32) -> bool {
        self.fields.iter().any(|(number, _)| *number == field)
    }

    fn bytes(&self, field: u32) -> &'a [u8] {
        self.fields
            .iter()
            .find_map(|(number, value)| match value {
                Wire::Bytes(bytes) if *number == field => Some(*bytes),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn string(&self, field: u32) -> String {
        String::from_utf8_lossy(self.bytes(field)).into_owned()
    }

    fn message(&self, field: u32) -> Message<'a> {
        Message::parse(self.bytes(field))
    }

    fn repeated(&self, field: u32) -> Vec<Message<'a>> {
        self.fields
            .iter()
            .filter_map(|(number, value)| match value {
                Wire::Bytes(bytes) if *number == field => Some(Message::parse(bytes)),
                _ => None,
            })
            .collect()
    }

    /// A varint field; absent means the protobuf default, zero.
    fn varint(&self, field: u32) -> u64 {
        self.fields
            .iter()
            .find_map(|(number, value)| match value {
                Wire::Varint(int) if *number == field => Some(*int),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// A fixed64 field; absent means the protobuf default, zero.
    fn fixed64(&self, field: u32) -> u64 {
        self.fields
            .iter()
            .find_map(|(number, value)| match value {
                Wire::Fixed64(int) if *number == field => Some(*int),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// A fixed32 field; absent means the protobuf default, zero.
    fn fixed32(&self, field: u32) -> u32 {
        self.fields
            .iter()
            .find_map(|(number, value)| match value {
                Wire::Fixed32(int) if *number == field => Some(*int),
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn read_varint(buf: &mut &[u8]) -> u64 {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let (&byte, rest) = buf.split_first().expect("truncated protobuf varint");
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
        assert!(shift < 64, "protobuf varint too long");
    }
}

fn take<const N: usize>(buf: &mut &[u8]) -> [u8; N] {
    assert!(N <= buf.len(), "truncated protobuf fixed-width field");
    let (head, rest) = buf.split_at(N);
    *buf = rest;
    head.try_into().expect("split_at returned N bytes")
}
