pub(crate) fn sse_event(event_type: &str, data: serde_json::Value) -> String {
    let data_str = serde_json::to_string(&data).unwrap_or_default();
    format!("event: {event_type}\ndata: {data_str}\n\n")
}
