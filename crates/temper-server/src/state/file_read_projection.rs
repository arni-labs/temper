use crate::storage::QueryProjectionFieldsRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileProjectionMeta {
    pub(super) content_hash: String,
    pub(super) mime_type: String,
    pub(super) has_content: bool,
    pub(super) size_bytes: Option<u64>,
}

fn size_from_text(value: Option<&str>) -> Option<u64> {
    let parsed = value?.parse::<i64>().ok()?;
    u64::try_from(parsed).ok()
}

fn size_from_json(fields: &serde_json::Value) -> Option<u64> {
    let value = fields.get("size_bytes")?;
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|size| u64::try_from(size).ok()))
}

pub(super) fn file_projection_from_row(row: QueryProjectionFieldsRow) -> FileProjectionMeta {
    FileProjectionMeta {
        content_hash: row
            .fields
            .get("content_hash")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        mime_type: row
            .fields
            .get("mime_type")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        has_content: row
            .fields
            .get("has_content")
            .and_then(|value| value.as_deref())
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        size_bytes: size_from_text(
            row.fields
                .get("size_bytes")
                .and_then(|value| value.as_deref()),
        ),
    }
}

pub(super) fn file_projection_from_state(fields: &serde_json::Value) -> FileProjectionMeta {
    FileProjectionMeta {
        content_hash: fields
            .get("content_hash")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        mime_type: fields
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        has_content: fields
            .get("has_content")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        size_bytes: size_from_json(fields),
    }
}

pub(super) fn file_version_projection_from_row(
    row: QueryProjectionFieldsRow,
) -> FileProjectionMeta {
    let content_hash = row
        .fields
        .get("content_hash")
        .cloned()
        .flatten()
        .unwrap_or_default();
    FileProjectionMeta {
        has_content: !content_hash.is_empty(),
        content_hash,
        mime_type: row
            .fields
            .get("mime_type")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        size_bytes: size_from_text(
            row.fields
                .get("size_bytes")
                .and_then(|value| value.as_deref()),
        ),
    }
}

pub(super) fn file_version_projection_from_state(fields: &serde_json::Value) -> FileProjectionMeta {
    let content_hash = fields
        .get("content_hash")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    FileProjectionMeta {
        has_content: !content_hash.is_empty(),
        content_hash,
        mime_type: fields
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        size_bytes: size_from_json(fields),
    }
}
